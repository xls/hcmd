//! What a protocol has to be able to do for [`crate::remote::fs::RemoteFs`].
//!
//!
//! Two implementations: [`crate::remote::sftp::SftpFs`] and
//! [`crate::remote::ftp::FtpFs`]. Everything above them - the listing cache,
//! the `..` row, the path arithmetic, the `Vfs` surface - is written once in
//! `fs.rs`, so the two protocols cannot drift apart in the half of the work
//! that is not protocol-specific.
//!
//! This module knows nothing about panels, tabs or dialogs.

use crate::error::Result;
use crate::remote::Protocol;
use crate::vfs::{Capabilities, Entry, ReadSeek};

/// What a protocol has to be able to do for [`crate::remote::fs::RemoteFs`].
///
/// **Every method blocks. Call from the blocking pool only**,
/// with the two exceptions that say otherwise on
/// themselves: [`RemoteTransport::capabilities`] and
/// [`RemoteTransport::is_live`] read values and never touch a socket.
///
/// Paths are `&str` in the remote's own namespace, always absolute, always
/// `/`-separated: that is what both protocols speak, and converting at this
/// boundary keeps `PathBuf`'s local-filesystem assumptions out of the wire
/// format.
pub trait RemoteTransport: Send + Sync {
    /// Which protocol this is.
    fn protocol(&self) -> Protocol;

    /// What this connection can do. **Never does I/O**: it answers from a
    /// value fixed when the connection was established, because
    /// `App::apply_vfs_event` asks on the event loop and a call that waited on
    /// a socket there would stall the frame.
    fn capabilities(&self) -> Capabilities;

    /// One directory.
    ///
    /// **Blocking.** Call from the blocking pool only.
    ///
    ///
    /// The `..` row is **not** included; [`crate::remote::fs::RemoteFs`] adds
    /// it, the way every other backend does.
    fn list(&self, dir: &str) -> Result<Vec<Entry>>;

    /// One path's metadata.
    ///
    /// **Blocking.** Call from the blocking pool only.
    ///
    fn stat(&self, path: &str) -> Result<Entry>;

    /// Where a symbolic link points.
    ///
    /// **Blocking.** Call from the blocking pool only.
    ///
    fn read_link(&self, path: &str) -> Result<String>;

    /// A pipelined, forward-only reader.
    ///
    /// **Blocking.** Call from the blocking pool only.
    ///
    fn open_read(&self, path: &str) -> Result<Box<dyn std::io::Read + Send>>;

    /// A seekable reader, for the viewer.
    ///
    /// **Blocking.** Call from the blocking pool only.
    ///
    ///
    /// `Err(Unsupported)` where `capabilities().seekable` is false, and the
    /// two must agree.
    fn open_seek(&self, path: &str) -> Result<Box<dyn ReadSeek + Send>>;

    /// A writer whose `flush` is the commit.
    ///
    /// **Blocking.** Call from the blocking pool only.
    ///
    fn open_write(&self, path: &str) -> Result<Box<dyn std::io::Write + Send>>;

    /// Create one directory.
    ///
    /// **Blocking.** Call from the blocking pool only.
    ///
    fn create_dir(&self, path: &str) -> Result<()>;

    /// Remove one file.
    ///
    /// **Blocking.** Call from the blocking pool only.
    ///
    fn remove_file(&self, path: &str) -> Result<()>;

    /// Remove one empty directory.
    ///
    /// **Blocking.** Call from the blocking pool only.
    ///
    fn remove_dir(&self, path: &str) -> Result<()>;

    /// Rename, server side.
    ///
    /// **Blocking.** Call from the blocking pool only.
    ///
    fn rename(&self, from: &str, to: &str) -> Result<()>;

    /// False once the connection is gone.
    ///
    /// **Never does I/O**: it reads a flag the actor sets, because the panel
    /// asks on every frame.
    fn is_live(&self) -> bool;

    /// Close every channel and stop the actor. Idempotent.
    fn close(&self);
}

/// An in-memory transport with injectable failures, for the `Vfs` tests.
///
///
/// It is what every test of [`crate::remote::fs::RemoteFs`] runs against, so
/// the whole `Vfs` surface, the cache, the `..` row and
/// [`crate::Error::ConnectionLost`] stopping a batch are all tested with no
/// server anywhere.
#[cfg(test)]
#[derive(Debug)]
pub struct FakeTransport {
    /// Which protocol it claims to be, so the FTP-shaped rules can be tested.
    protocol: Protocol,
    /// What it claims to be able to do.
    caps: Capabilities,
    /// The tree, by absolute path. Directories hold no bytes.
    ///
    /// An `Arc` because [`FakeWriter`] outlives the borrow that made it: a
    /// `Box<dyn Write + Send>` carries no lifetime, so the writer owns a
    /// handle to the tree rather than a reference into the transport.
    tree: std::sync::Arc<std::sync::Mutex<std::collections::BTreeMap<String, Node>>>,
    /// False once a `drop_connection_at` path has been touched.
    live: std::sync::atomic::AtomicBool,
    /// Operations counted, for `fail_after`.
    calls: std::sync::atomic::AtomicUsize,
    /// Fail every operation past this many.
    fail_after: std::sync::atomic::AtomicUsize,
    /// Drop the connection when this path is opened.
    drop_at: std::sync::Mutex<Option<String>>,
    /// How long `list` sleeps, for the "returns its receiver promptly" test.
    list_delay: std::sync::Mutex<std::time::Duration>,
    /// Extra listing rows, by directory, whose *names* the "server" chose
    /// (see [`FakeTransport::with_listing_name`]).
    injected: std::sync::Mutex<Vec<(String, Entry)>>,
    /// The largest buffer any reader was handed, for the chunk-size test
    /// (the design I12).
    max_read: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

/// One node of [`FakeTransport`]'s tree.
#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
enum Node {
    /// A directory.
    Dir,
    /// A file and its bytes.
    File(Vec<u8>),
    /// A symbolic link and its target.
    Link(String),
}

#[cfg(test)]
impl FakeTransport {
    /// An empty SFTP-shaped transport with a root directory.
    pub fn new() -> Self {
        let mut tree = std::collections::BTreeMap::new();
        tree.insert("/".to_string(), Node::Dir);
        Self {
            protocol: Protocol::Sftp,
            caps: Capabilities::SFTP,
            tree: std::sync::Arc::new(std::sync::Mutex::new(tree)),
            live: std::sync::atomic::AtomicBool::new(true),
            calls: std::sync::atomic::AtomicUsize::new(0),
            fail_after: std::sync::atomic::AtomicUsize::new(usize::MAX),
            drop_at: std::sync::Mutex::new(None),
            list_delay: std::sync::Mutex::new(std::time::Duration::ZERO),
            injected: std::sync::Mutex::new(Vec::new()),
            max_read: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    /// The same, claiming to be FTP: no seeking, no modes, no links.
    pub fn ftp() -> Self {
        Self {
            protocol: Protocol::Ftp,
            caps: Capabilities::FTP,
            ..Self::new()
        }
    }

    /// Add a directory, and every directory above it.
    pub fn with_dir(self, path: &str) -> Self {
        self.map().insert(path.to_string(), Node::Dir);
        self
    }

    /// Add a file with contents.
    pub fn with_file(self, path: &str, bytes: &[u8]) -> Self {
        self.map()
            .insert(path.to_string(), Node::File(bytes.to_vec()));
        self
    }

    /// Add a symbolic link.
    pub fn with_link(self, path: &str, target: &str) -> Self {
        self.map()
            .insert(path.to_string(), Node::Link(target.to_string()));
        self
    }

    /// Fail every operation past the `n`th.
    pub fn fail_after(self, n: usize) -> Self {
        self.fail_after
            .store(n, std::sync::atomic::Ordering::SeqCst);
        self
    }

    /// Lose the connection the moment this path is opened for reading.
    pub fn drop_connection_at(self, path: &str) -> Self {
        *self.lock_drop_at() = Some(path.to_string());
        self
    }

    /// Make `list` take this long, for the "read_dir returns promptly" test.
    pub fn with_list_delay(self, delay: std::time::Duration) -> Self {
        *self.lock_delay() = delay;
        self
    }

    /// Answer `list(dir)` with a row whose **name is whatever the server
    /// says** (`../../escaped.txt`, `/etc/cron.d/pwn`), and serve its bytes
    /// under the path that name joins to.
    ///
    /// A well-behaved server sends a file name here; a hostile one sends a
    /// path, aiming at the `dst.join(&child.name)` a recursive copy does.
    /// The bytes are reachable on purpose: a test that
    /// asserts nothing was written outside the destination proves nothing
    /// unless the write would have succeeded.
    pub fn with_listing_name(self, dir: &str, name: &str, bytes: &[u8]) -> Self {
        self.lock_injected()
            .push((dir.to_string(), Entry::file(name)));
        self.map()
            .insert(format!("{dir}/{name}"), Node::File(bytes.to_vec()));
        self
    }

    /// The tree, with a poisoned lock recovered rather than escalated.
    fn map(&self) -> std::sync::MutexGuard<'_, std::collections::BTreeMap<String, Node>> {
        self.tree
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The injected rows.
    fn lock_injected(&self) -> std::sync::MutexGuard<'_, Vec<(String, Entry)>> {
        self.injected
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The drop-at path.
    fn lock_drop_at(&self) -> std::sync::MutexGuard<'_, Option<String>> {
        self.drop_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The list delay.
    fn lock_delay(&self) -> std::sync::MutexGuard<'_, std::time::Duration> {
        self.list_delay
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Count one operation and refuse past the injected limit.
    fn charge(&self, path: &str) -> Result<()> {
        if !self.is_live() {
            return Err(crate::Error::ConnectionLost("fake://host".to_string()));
        }
        let n = self
            .calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            .saturating_add(1);
        if n > self.fail_after.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(crate::Error::msg(format!("{path}: injected failure")));
        }
        Ok(())
    }

    /// How many operations have been attempted.
    pub fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// The largest read buffer any consumer handed one of this transport's
    /// readers, which is what `ops::chunk_size` decides
    /// (the design I12).
    pub fn max_read(&self) -> usize {
        self.max_read.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// One row for a node.
    fn entry_of(name: &str, node: &Node) -> Entry {
        match node {
            Node::Dir => Entry {
                mode: 0o040755,
                ..Entry::dir(name)
            },
            Node::File(bytes) => Entry {
                size: bytes.len() as u64,
                mode: 0o100644,
                ..Entry::file(name)
            },
            Node::Link(_) => Entry {
                kind: crate::vfs::EntryKind::Symlink { to_dir: false },
                ..Entry::file(name)
            },
        }
    }
}

#[cfg(test)]
impl Default for FakeTransport {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
impl RemoteTransport for FakeTransport {
    fn protocol(&self) -> Protocol {
        self.protocol
    }

    fn capabilities(&self) -> Capabilities {
        self.caps
    }

    fn list(&self, dir: &str) -> Result<Vec<Entry>> {
        self.charge(dir)?;
        let delay = *self.lock_delay();
        if !delay.is_zero() {
            std::thread::sleep(delay);
        }
        let map = self.map();
        if map.get(dir) != Some(&Node::Dir) {
            return Err(crate::Error::NotFound(dir.to_string()));
        }
        let prefix = if dir.ends_with('/') {
            dir.to_string()
        } else {
            format!("{dir}/")
        };
        let mut rows = Vec::new();
        for (path, node) in map.iter() {
            let Some(rest) = path.strip_prefix(&prefix) else {
                continue;
            };
            if rest.is_empty() || rest.contains('/') {
                continue;
            }
            rows.push(Self::entry_of(rest, node));
        }
        for (at, entry) in self.lock_injected().iter() {
            if at == dir {
                rows.push(entry.clone());
            }
        }
        Ok(rows)
    }

    fn stat(&self, path: &str) -> Result<Entry> {
        self.charge(path)?;
        let map = self.map();
        let node = map
            .get(path)
            .ok_or_else(|| crate::Error::NotFound(path.to_string()))?;
        let name = path.rsplit('/').next().unwrap_or(path);
        Ok(Self::entry_of(name, node))
    }

    fn read_link(&self, path: &str) -> Result<String> {
        self.charge(path)?;
        match self.map().get(path) {
            Some(Node::Link(target)) => Ok(target.clone()),
            Some(_) => Err(crate::Error::msg(format!("{path}: not a symbolic link"))),
            None => Err(crate::Error::NotFound(path.to_string())),
        }
    }

    fn open_read(&self, path: &str) -> Result<Box<dyn std::io::Read + Send>> {
        if self.lock_drop_at().as_deref() == Some(path) {
            self.live.store(false, std::sync::atomic::Ordering::SeqCst);
            return Err(crate::Error::ConnectionLost("fake://host".to_string()));
        }
        self.charge(path)?;
        match self.map().get(path) {
            Some(Node::File(bytes)) => Ok(Box::new(RecordingReader {
                inner: std::io::Cursor::new(bytes.clone()),
                max_read: std::sync::Arc::clone(&self.max_read),
            })),
            Some(_) => Err(crate::Error::msg(format!("{path}: not a regular file"))),
            None => Err(crate::Error::NotFound(path.to_string())),
        }
    }

    fn open_seek(&self, path: &str) -> Result<Box<dyn ReadSeek + Send>> {
        if !self.caps.seekable {
            return Err(crate::Error::Unsupported("random-access reading"));
        }
        self.charge(path)?;
        match self.map().get(path) {
            Some(Node::File(bytes)) => Ok(Box::new(std::io::Cursor::new(bytes.clone()))),
            Some(_) => Err(crate::Error::msg(format!("{path}: not a regular file"))),
            None => Err(crate::Error::NotFound(path.to_string())),
        }
    }

    fn open_write(&self, path: &str) -> Result<Box<dyn std::io::Write + Send>> {
        self.charge(path)?;
        Ok(Box::new(FakeWriter {
            path: path.to_string(),
            buffer: Vec::new(),
            tree: std::sync::Arc::clone(&self.tree),
        }))
    }

    fn create_dir(&self, path: &str) -> Result<()> {
        self.charge(path)?;
        self.map().insert(path.to_string(), Node::Dir);
        Ok(())
    }

    fn remove_file(&self, path: &str) -> Result<()> {
        self.charge(path)?;
        self.map()
            .remove(path)
            .map(|_| ())
            .ok_or_else(|| crate::Error::NotFound(path.to_string()))
    }

    fn remove_dir(&self, path: &str) -> Result<()> {
        self.remove_file(path)
    }

    fn rename(&self, from: &str, to: &str) -> Result<()> {
        self.charge(from)?;
        let mut map = self.map();
        let node = map
            .remove(from)
            .ok_or_else(|| crate::Error::NotFound(from.to_string()))?;
        map.insert(to.to_string(), node);
        Ok(())
    }

    fn is_live(&self) -> bool {
        self.live.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn close(&self) {
        self.live.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

/// A reader that remembers the largest buffer it was handed.
///
/// It is how the design I12 is testable at all: "`ops::chunk_size`
/// is what sizes the copy buffer on both VFS paths" is a statement about the
/// argument to `Read::read`, and this is the only place that argument can be
/// observed.
#[cfg(test)]
struct RecordingReader {
    /// The bytes.
    inner: std::io::Cursor<Vec<u8>>,
    /// The largest `buf.len()` seen so far.
    max_read: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

#[cfg(test)]
impl std::io::Read for RecordingReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.max_read
            .fetch_max(buf.len(), std::sync::atomic::Ordering::SeqCst);
        std::io::Read::read(&mut self.inner, buf)
    }
}

/// [`FakeTransport`]'s writer: the bytes land in the tree on `flush`, which is
/// what makes the fake agree with the real backends about `flush` being the
/// commit.
#[cfg(test)]
struct FakeWriter {
    /// Where the bytes are going.
    path: String,
    /// What has been written so far.
    buffer: Vec<u8>,
    /// The tree to commit into.
    tree: std::sync::Arc<std::sync::Mutex<std::collections::BTreeMap<String, Node>>>,
}

#[cfg(test)]
impl std::io::Write for FakeWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.buffer.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let mut map = self
            .tree
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        map.insert(
            self.path.clone(),
            Node::File(std::mem::take(&mut self.buffer)),
        );
        Ok(())
    }
}
