//! The connection actor: the one task in the tree that awaits a socket.
//!
//!
//! Everything above it is synchronous and lives on the blocking pool. A
//! transport method builds a [`Command`], sends it down a
//! `tokio::sync::mpsc::UnboundedSender` (whose `send` is neither `async` nor
//! blocking nor able to panic) and parks on a `std::sync::mpsc::Receiver`
//! (whose `recv` blocks any thread and panics on none). Those two choices are
//! the design's, and between them there is no call site anyone
//! can add later that turns into a panic.
//!
//! The loop itself does no I/O beyond dispatch: each command is handed to a
//! `tokio::spawn`ed task with a pooled channel, so a two-gigabyte download
//! does not stop a directory listing ("so that a copy in the
//! background and browsing in the foreground do not fight over one channel").

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::SyncSender;

use russh_sftp::client::RawSftpSession;
use russh_sftp::client::error::Error as SftpError;
use russh_sftp::protocol::{FileAttributes, OpenFlags, StatusCode};
use tokio::sync::Semaphore;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
use tokio::task::JoinHandle;

use crate::error::{Error, Result};
use crate::vfs::{Entry, EntryKind};

use super::attrs::{self, SAFE_PAYLOAD};
use super::io::{Chunk, PipelinedReader, PipelinedWriter, WindowReader, WindowRequest, WriteMsg};

/// Where one command's answer goes.
///
/// A `SyncSender` of depth one, because the thread that sent the command is
/// parked on the matching `recv` and has at most one command in flight.
///
pub(crate) type Reply<T> = SyncSender<Result<T>>;

/// What the actor is asked to do.
///
/// One variant per [`super::super::RemoteTransport`] method, plus the two that
/// only exist inside this backend: `RealPath`, which is how the login
/// directory is found when a target names none, and `SetPermissions`, which is
/// the mode preservation on the remote side.
///
/// the design sketches this enum with a single `Open`; it is
/// three here because the three handles are three different shapes
/// (a pipeline, a window server and a write queue) and one variant carrying an
/// enum of them would be decoded straight back apart in the actor.
pub(crate) enum Command {
    /// One directory, without the `.` and `..` the server sends.
    List {
        /// The directory.
        dir: String,
        /// The answer.
        reply: Reply<Vec<Entry>>,
    },
    /// `SSH_FXP_LSTAT`: one path, links not followed.
    Stat {
        /// The path.
        path: String,
        /// The answer.
        reply: Reply<Entry>,
    },
    /// `SSH_FXP_READLINK`.
    ReadLink {
        /// The link.
        path: String,
        /// The answer.
        reply: Reply<String>,
    },
    /// `SSH_FXP_REALPATH`, which is how "wherever the server puts us" is
    /// turned into a directory a panel can show.
    RealPath {
        /// The path, `.` for the login directory.
        path: String,
        /// The answer.
        reply: Reply<String>,
    },
    /// A pipelined forward-only reader.
    OpenRead {
        /// The file.
        path: String,
        /// The answer.
        reply: Reply<PipelinedReader>,
    },
    /// A window server for the viewer.
    OpenSeek {
        /// The file.
        path: String,
        /// The answer.
        reply: Reply<WindowReader>,
    },
    /// A write queue whose flush is the commit.
    OpenWrite {
        /// The file.
        path: String,
        /// The answer.
        reply: Reply<PipelinedWriter>,
    },
    /// `SSH_FXP_MKDIR`.
    Mkdir {
        /// The directory.
        path: String,
        /// The answer.
        reply: Reply<()>,
    },
    /// `SSH_FXP_REMOVE`.
    RemoveFile {
        /// The file.
        path: String,
        /// The answer.
        reply: Reply<()>,
    },
    /// `SSH_FXP_RMDIR`.
    RemoveDir {
        /// The directory.
        path: String,
        /// The answer.
        reply: Reply<()>,
    },
    /// `SSH_FXP_RENAME`.
    Rename {
        /// From.
        from: String,
        /// To.
        to: String,
        /// The answer.
        reply: Reply<()>,
    },
    /// `SSH_FXP_SETSTAT` with permissions only.
    SetPermissions {
        /// The path.
        path: String,
        /// The mode bits.
        mode: u32,
        /// The answer.
        reply: Reply<()>,
    },
    /// Stop. Idempotent, because the actor also stops when its sender drops.
    Close,
}

/// The largest payload the server will accept, per direction.
///
/// Filled in from `limits@openssh.com` where the server announces it and left
/// at [`SAFE_PAYLOAD`] where it does not. This is where the design's
/// throughput actually comes from: an OpenSSH server raises both to 256 KiB,
/// which is eight times the floor every server has to accept.
#[derive(Debug, Clone, Copy)]
pub(crate) struct WireLimits {
    /// The largest `SSH_FXP_READ`.
    pub(crate) read: usize,
    /// The largest `SSH_FXP_WRITE`.
    pub(crate) write: usize,
}

impl Default for WireLimits {
    fn default() -> Self {
        Self {
            read: SAFE_PAYLOAD,
            write: SAFE_PAYLOAD,
        }
    }
}

/// The SFTP channels one connection opened.
///
/// Channels on **one** SSH connection, not a second handshake each: SSH
/// multiplexes, and one authentication is what the user asked for when they
/// connected once.
pub(crate) struct Pool {
    sessions: Vec<Arc<RawSftpSession>>,
    next: AtomicUsize,
}

impl Pool {
    /// Wrap the channels a connection opened.
    pub(crate) fn new(sessions: Vec<Arc<RawSftpSession>>) -> Self {
        Self {
            sessions,
            next: AtomicUsize::new(0),
        }
    }

    /// The next channel, round robin.
    ///
    /// `None` only for an empty pool, which is a connection that has been torn
    /// down; the caller turns that into [`Error::ConnectionLost`] rather than
    /// indexing into nothing.
    pub(crate) fn take(&self) -> Option<Arc<RawSftpSession>> {
        if self.sessions.is_empty() {
            return None;
        }
        let at = self.next.fetch_add(1, Ordering::Relaxed);
        self.sessions.get(at % self.sessions.len()).cloned()
    }

    /// Close every channel.
    pub(crate) async fn shutdown(&self) {
        for session in &self.sessions {
            let _ = session.close_session();
        }
    }
}

/// What every spawned command task needs and nothing more.
///
/// Shared behind an `Arc` so that dispatch costs a refcount rather than a
/// clone of the authority string per command.
pub(crate) struct Context {
    /// `sftp://thorin@nas.local:2222`, for [`Error::ConnectionLost`] and for
    /// nothing that could carry a secret.
    pub(crate) authority: String,
    /// The wire limits this server announced.
    pub(crate) limits: WireLimits,
    /// How many requests one transfer keeps in flight (`remote.pipeline`).
    pub(crate) depth: usize,
    /// The read-ahead window, from `ops::chunk_size` and the wire limit.
    pub(crate) window: usize,
    /// Cleared the moment the connection is known to be gone.
    pub(crate) live: Arc<AtomicBool>,
}

impl Context {
    /// Mark the connection gone, once, and return the error that says so.
    fn lost(&self) -> Error {
        self.live.store(false, Ordering::Release);
        Error::ConnectionLost(self.authority.clone())
    }

    /// Note whether a failure was the connection rather than the file.
    fn observe(&self, err: &Error) {
        if matches!(err, Error::ConnectionLost(_)) {
            self.live.store(false, Ordering::Release);
        }
    }
}

/// Run the actor until it is closed or its sender is dropped.
///
/// `handle` is held for its whole life and never used except to notice the
/// connection dying and to say goodbye politely at the end; dropping it early
/// would tear down the SSH session under the pool.
pub(crate) async fn run<H: russh::client::Handler>(
    mut rx: UnboundedReceiver<Command>,
    pool: Arc<Pool>,
    context: Arc<Context>,
    handle: russh::client::Handle<H>,
) {
    while let Some(command) = rx.recv().await {
        if matches!(command, Command::Close) {
            break;
        }
        // A command issued after the link died is answered immediately rather
        // than waiting for a request that will never come back
        // (the disconnected state).
        if handle.is_closed() {
            context.live.store(false, Ordering::Release);
        }
        let Some(session) = pool.take().filter(|_| context.live.load(Ordering::Acquire)) else {
            refuse(command, &context);
            continue;
        };
        let ctx = Arc::clone(&context);
        // Spawned rather than awaited: the loop must be back on `recv` before
        // the first byte of a listing arrives, or a download would serialise
        // the whole panel behind it.
        tokio::spawn(async move { dispatch(session, command, ctx).await });
    }
    context.live.store(false, Ordering::Release);
    pool.shutdown().await;
    let _ = handle
        .disconnect(russh::Disconnect::ByApplication, "", "")
        .await;
}

/// Answer every command with [`Error::ConnectionLost`].
///
/// Written out per variant rather than with a wildcard, because `Command` is
/// this crate's enum and a variant added later must be a build error here and
/// not a silently dropped reply (the house rule on exhaustive matches).
fn refuse(command: Command, context: &Context) {
    match command {
        Command::List { reply, .. } => {
            let _ = reply.send(Err(context.lost()));
        }
        Command::Stat { reply, .. } => {
            let _ = reply.send(Err(context.lost()));
        }
        Command::ReadLink { reply, .. } | Command::RealPath { reply, .. } => {
            let _ = reply.send(Err(context.lost()));
        }
        Command::OpenRead { reply, .. } => {
            let _ = reply.send(Err(context.lost()));
        }
        Command::OpenSeek { reply, .. } => {
            let _ = reply.send(Err(context.lost()));
        }
        Command::OpenWrite { reply, .. } => {
            let _ = reply.send(Err(context.lost()));
        }
        Command::Mkdir { reply, .. }
        | Command::RemoveFile { reply, .. }
        | Command::RemoveDir { reply, .. }
        | Command::Rename { reply, .. }
        | Command::SetPermissions { reply, .. } => {
            let _ = reply.send(Err(context.lost()));
        }
        Command::Close => {}
    }
}

/// Perform one command on one pooled channel and answer it.
async fn dispatch(session: Arc<RawSftpSession>, command: Command, ctx: Arc<Context>) {
    match command {
        Command::List { dir, reply } => {
            let out = list(&session, &dir, &ctx).await;
            answer(reply, out, &ctx);
        }
        Command::Stat { path, reply } => {
            let out = stat(&session, &path, &ctx).await;
            answer(reply, out, &ctx);
        }
        Command::ReadLink { path, reply } => {
            let out = first_name(session.readlink(path.clone()).await, &path, &ctx);
            answer(reply, out, &ctx);
        }
        Command::RealPath { path, reply } => {
            let out = first_name(session.realpath(path.clone()).await, &path, &ctx);
            answer(reply, out, &ctx);
        }
        Command::OpenRead { path, reply } => {
            let out = open_read(session, &path, &ctx).await;
            answer(reply, out, &ctx);
        }
        Command::OpenSeek { path, reply } => {
            let out = open_seek(session, &path, &ctx).await;
            answer(reply, out, &ctx);
        }
        Command::OpenWrite { path, reply } => {
            let out = open_write(session, &path, &ctx).await;
            answer(reply, out, &ctx);
        }
        Command::Mkdir { path, reply } => {
            let out = session
                .mkdir(path.clone(), FileAttributes::empty())
                .await
                .map(|_| ())
                .map_err(|e| attrs::map_error(&path, &ctx.authority, &e));
            answer(reply, out, &ctx);
        }
        Command::RemoveFile { path, reply } => {
            let out = session
                .remove(path.clone())
                .await
                .map(|_| ())
                .map_err(|e| attrs::map_error(&path, &ctx.authority, &e));
            answer(reply, out, &ctx);
        }
        Command::RemoveDir { path, reply } => {
            let out = session
                .rmdir(path.clone())
                .await
                .map(|_| ())
                .map_err(|e| attrs::map_error(&path, &ctx.authority, &e));
            answer(reply, out, &ctx);
        }
        Command::Rename { from, to, reply } => {
            let out = session
                .rename(from.clone(), to)
                .await
                .map(|_| ())
                .map_err(|e| attrs::map_error(&from, &ctx.authority, &e));
            answer(reply, out, &ctx);
        }
        Command::SetPermissions { path, mode, reply } => {
            let mut wanted = FileAttributes::empty();
            wanted.permissions = Some(mode);
            let out = session
                .setstat(path.clone(), wanted)
                .await
                .map(|_| ())
                .map_err(|e| attrs::map_error(&path, &ctx.authority, &e));
            answer(reply, out, &ctx);
        }
        // Handled by the loop; it never reaches a task.
        Command::Close => {}
    }
}

/// Send one answer, noting a lost connection on the way past.
///
/// A failed send is the caller having given up - `Esc` on a job, a panel that
/// moved on - and is not an error here.
fn answer<T>(reply: Reply<T>, out: Result<T>, ctx: &Context) {
    if let Err(err) = &out {
        ctx.observe(err);
    }
    let _ = reply.send(out);
}

/// The single name in a `SSH_FXP_NAME` reply.
fn first_name(
    reply: std::result::Result<russh_sftp::protocol::Name, SftpError>,
    path: &str,
    ctx: &Context,
) -> Result<String> {
    match reply {
        Ok(name) => match name.files.first() {
            Some(file) => Ok(file.filename.clone()),
            None => Err(Error::msg(format!("{path}: the server named nothing"))),
        },
        Err(err) => Err(attrs::map_error(path, &ctx.authority, &err)),
    }
}

/// One directory.
async fn list(session: &Arc<RawSftpSession>, dir: &str, ctx: &Context) -> Result<Vec<Entry>> {
    let handle = session
        .opendir(dir.to_string())
        .await
        .map_err(|e| attrs::map_error(dir, &ctx.authority, &e))?
        .handle;
    let mut out = Vec::new();
    let mut failure = None;
    loop {
        match session.readdir(handle.as_str()).await {
            Ok(name) => {
                for file in name.files {
                    if attrs::is_not_a_row(&file.filename) {
                        continue;
                    }
                    out.push(attrs::entry_from(&file.filename, &file.attrs));
                }
            }
            Err(SftpError::Status(status)) if status.status_code == StatusCode::Eof => break,
            Err(err) => {
                failure = Some(attrs::map_error(dir, &ctx.authority, &err));
                break;
            }
        }
    }
    let _ = session.close(handle).await;
    if let Some(err) = failure {
        return Err(err);
    }
    resolve_links(session, dir, &mut out, ctx).await;
    Ok(out)
}

/// Ask what each symbolic link in a listing points at.
///
/// The panel needs `to_dir` to decide whether `Enter` descends, and a listing
/// cannot know: SFTP's `READDIR` carries `lstat` attributes. One
/// `SSH_FXP_STAT` per link is the honest cost, and it is paid `depth` at a
/// time so a directory of a thousand links costs a thousand requests rather
/// than a thousand round trips.
///
/// A link that cannot be resolved keeps `to_dir: false`, which is exactly what
/// `LocalFs` reports for a broken link.
async fn resolve_links(
    session: &Arc<RawSftpSession>,
    dir: &str,
    rows: &mut [Entry],
    ctx: &Context,
) {
    let links: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, row)| row.is_symlink())
        .map(|(at, _)| at)
        .collect();
    for batch in links.chunks(ctx.depth.max(1)) {
        let mut running: Vec<(usize, JoinHandle<_>)> = Vec::new();
        for at in batch {
            let Some(row) = rows.get(*at) else { continue };
            let path = attrs::join(dir, &row.name);
            let session = Arc::clone(session);
            running.push((*at, tokio::spawn(async move { session.stat(path).await })));
        }
        for (at, task) in running {
            if let Ok(Ok(reply)) = task.await
                && reply.attrs.is_dir()
                && let Some(row) = rows.get_mut(at)
            {
                row.kind = EntryKind::Symlink { to_dir: true };
            }
        }
    }
}

/// One path's metadata, links not followed.
async fn stat(session: &Arc<RawSftpSession>, path: &str, ctx: &Context) -> Result<Entry> {
    let reply = session
        .lstat(path.to_string())
        .await
        .map_err(|e| attrs::map_error(path, &ctx.authority, &e))?;
    let name = path.rsplit('/').next().unwrap_or(path);
    let mut entry = attrs::entry_from(name, &reply.attrs);
    // A `stat` of a link is asked for by name, so resolving it costs one more
    // round trip and answers the question the caller actually has.
    if entry.is_symlink()
        && let Ok(target) = session.stat(path.to_string()).await
        && target.attrs.is_dir()
    {
        entry.kind = EntryKind::Symlink { to_dir: true };
    }
    Ok(entry)
}

/// Open a pipelined forward-only reader.
async fn open_read(
    session: Arc<RawSftpSession>,
    path: &str,
    ctx: &Context,
) -> Result<PipelinedReader> {
    let handle = session
        .open(path.to_string(), OpenFlags::READ, FileAttributes::empty())
        .await
        .map_err(|e| attrs::map_error(path, &ctx.authority, &e))?
        .handle;
    let (tx, rx) = std::sync::mpsc::channel::<Chunk>();
    let permits = Arc::new(Semaphore::new(ctx.depth.max(1)));
    let reader = PipelinedReader::new(rx, Arc::clone(&permits));
    let path = path.to_string();
    let authority = ctx.authority.clone();
    let window = ctx.window;
    let max = ctx.limits.read;
    let depth = ctx.depth.max(1);
    tokio::spawn(async move {
        pipeline_reads(
            session, handle, tx, permits, window, max, depth, path, authority,
        )
        .await;
    });
    Ok(reader)
}

/// The read-ahead itself.
///
/// `depth` windows are in flight at once and delivered in file order. A window
/// that comes back short is the end of the file - [`read_window`] has already
/// retried inside it, so a short answer is not a short read - and the requests
/// issued past it are abandoned rather than waited for.
async fn pipeline_reads(
    session: Arc<RawSftpSession>,
    handle: String,
    tx: std::sync::mpsc::Sender<Chunk>,
    permits: Arc<Semaphore>,
    window: usize,
    max: usize,
    depth: usize,
    path: String,
    authority: String,
) {
    let mut running: VecDeque<JoinHandle<std::result::Result<Vec<u8>, Error>>> = VecDeque::new();
    let mut offset = 0u64;
    let mut ended = false;
    loop {
        while !ended && running.len() < depth {
            // The permit is the read-ahead bound, and a closed semaphore is a
            // dropped reader: stop issuing and let the queue drain.
            match permits.acquire().await {
                Ok(permit) => permit.forget(),
                Err(_) => {
                    ended = true;
                    break;
                }
            }
            let at = offset;
            offset = offset.saturating_add(window as u64);
            let session = Arc::clone(&session);
            let handle = handle.clone();
            let path = path.clone();
            let authority = authority.clone();
            running.push_back(tokio::spawn(async move {
                read_window(&session, &handle, at, window, max, &path, &authority).await
            }));
        }
        let Some(front) = running.pop_front() else {
            break;
        };
        let outcome = match front.await {
            Ok(outcome) => outcome,
            Err(_) => Err(Error::ConnectionLost(authority.clone())),
        };
        match outcome {
            Ok(data) => {
                let short = data.len() < window;
                if !data.is_empty() && tx.send(Ok(data)).is_err() {
                    break;
                }
                if short {
                    ended = true;
                }
            }
            Err(err) => {
                let _ = tx.send(Err(err));
                break;
            }
        }
        if ended && running.is_empty() {
            break;
        }
    }
    for task in running {
        task.abort();
    }
    drop(tx);
    let _ = session.close(handle).await;
}

/// One window, retried across short answers.
///
/// SFTP allows a server to return fewer bytes than were asked for at any
/// offset, so "shorter than requested" only means end of file once this has
/// asked again and been told so. Getting that wrong truncates a copy, which is
/// the kind of failure that is not noticed until the file is needed.
async fn read_window(
    session: &RawSftpSession,
    handle: &str,
    offset: u64,
    want: usize,
    max: usize,
    path: &str,
    authority: &str,
) -> std::result::Result<Vec<u8>, Error> {
    let mut out: Vec<u8> = Vec::new();
    while out.len() < want {
        let remaining = want.saturating_sub(out.len());
        let len = u32::try_from(remaining.min(max.max(1))).unwrap_or(u32::MAX);
        let at = offset.saturating_add(out.len() as u64);
        match session.read(handle.to_string(), at, len).await {
            Ok(data) => {
                if data.data.is_empty() {
                    break;
                }
                out.extend_from_slice(&data.data);
            }
            Err(SftpError::Status(status)) if status.status_code == StatusCode::Eof => break,
            Err(err) => {
                let mapped = attrs::map_error(path, authority, &err);
                return Err(mapped);
            }
        }
    }
    Ok(out)
}

/// Open a window server for the viewer.
async fn open_seek(
    session: Arc<RawSftpSession>,
    path: &str,
    ctx: &Context,
) -> Result<WindowReader> {
    let handle = session
        .open(path.to_string(), OpenFlags::READ, FileAttributes::empty())
        .await
        .map_err(|e| attrs::map_error(path, &ctx.authority, &e))?
        .handle;
    // The size is asked for once, so `SeekFrom::End` costs no round trip
    // afterwards - which is the whole reason the viewer wants a seeking
    // handle rather than a stream.
    let size = session
        .fstat(handle.as_str())
        .await
        .ok()
        .and_then(|reply| reply.attrs.size)
        .unwrap_or(0);
    let (tx, mut rx) = unbounded_channel::<WindowRequest>();
    let reader = WindowReader::new(tx, size, ctx.window, ctx.authority.clone());
    let path = path.to_string();
    let authority = ctx.authority.clone();
    let max = ctx.limits.read;
    tokio::spawn(async move {
        while let Some(request) = rx.recv().await {
            let data = read_window(
                &session,
                &handle,
                request.offset,
                request.len,
                max,
                &path,
                &authority,
            )
            .await;
            // A dropped reply channel is a viewer that moved on.
            let _ = request.reply.send(data);
        }
        let _ = session.close(handle).await;
    });
    Ok(reader)
}

/// Open a write queue whose flush is the commit.
async fn open_write(
    session: Arc<RawSftpSession>,
    path: &str,
    ctx: &Context,
) -> Result<PipelinedWriter> {
    let handle = session
        .open(
            path.to_string(),
            OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::WRITE,
            FileAttributes::empty(),
        )
        .await
        .map_err(|e| attrs::map_error(path, &ctx.authority, &e))?
        .handle;
    let (tx, rx) = unbounded_channel::<WriteMsg>();
    let (acks, ack_rx) = std::sync::mpsc::channel::<Result<()>>();
    let writer = PipelinedWriter::new(
        tx,
        ack_rx,
        ctx.depth.max(1),
        path.to_string(),
        ctx.authority.clone(),
    );
    let path = path.to_string();
    let authority = ctx.authority.clone();
    let max = ctx.limits.write;
    let depth = ctx.depth.max(1);
    tokio::spawn(async move {
        pipeline_writes(session, handle, rx, acks, max, depth, path, authority).await;
    });
    Ok(writer)
}

/// The write pipeline.
///
/// At most `depth` chunks are unacknowledged at once, which is the bound
/// invariant I11 names; the writer enforces its half by refusing to send a
/// `depth + 1`th, and this enforces its half by collecting one before it
/// accepts another.
async fn pipeline_writes(
    session: Arc<RawSftpSession>,
    handle: String,
    mut rx: UnboundedReceiver<WriteMsg>,
    acks: std::sync::mpsc::Sender<Result<()>>,
    max: usize,
    depth: usize,
    path: String,
    authority: String,
) {
    let mut running: VecDeque<JoinHandle<std::result::Result<(), Error>>> = VecDeque::new();
    let mut offset = 0u64;
    let mut failed: Option<String> = None;
    let mut lost = false;
    loop {
        if running.len() >= depth {
            collect_write(&mut running, &acks, &mut failed, &mut lost, &authority).await;
            continue;
        }
        let Some(message) = rx.recv().await else {
            // The writer was dropped without a flush. Nothing is committed
            // and nobody is listening, so the handle is closed and the
            // failure, if any, goes nowhere: `ops::copy` calls `flush`, and
            // that is where a commit is reported.
            break;
        };
        match message {
            WriteMsg::Chunk(data) => {
                let at = offset;
                offset = offset.saturating_add(data.len() as u64);
                if failed.is_some() {
                    let _ = acks.send(Err(rebuild(&failed, lost, &authority, &path)));
                    continue;
                }
                let session = Arc::clone(&session);
                let handle = handle.clone();
                let path = path.clone();
                let authority = authority.clone();
                running.push_back(tokio::spawn(async move {
                    write_window(&session, &handle, at, data, max, &path, &authority).await
                }));
            }
            WriteMsg::Flush(reply) => {
                while !running.is_empty() {
                    collect_write(&mut running, &acks, &mut failed, &mut lost, &authority).await;
                }
                // The close is the commit: a server that failed to write
                // reports it here on many implementations, so its status is
                // part of the answer and not something to discard.
                let closed = session.close(handle.clone()).await;
                let outcome = match (&failed, closed) {
                    (Some(_), _) => Err(rebuild(&failed, lost, &authority, &path)),
                    (None, Err(err)) => Err(attrs::map_error(&path, &authority, &err)),
                    (None, Ok(_)) => Ok(()),
                };
                let _ = reply.send(outcome);
                return;
            }
        }
    }
    for task in running {
        task.abort();
    }
    let _ = session.close(handle).await;
}

/// Collect the oldest outstanding write and acknowledge it.
async fn collect_write(
    running: &mut VecDeque<JoinHandle<std::result::Result<(), Error>>>,
    acks: &std::sync::mpsc::Sender<Result<()>>,
    failed: &mut Option<String>,
    lost: &mut bool,
    authority: &str,
) {
    let Some(front) = running.pop_front() else {
        return;
    };
    let outcome = match front.await {
        Ok(outcome) => outcome,
        Err(_) => Err(Error::ConnectionLost(authority.to_string())),
    };
    match outcome {
        Ok(()) => {
            let _ = acks.send(Ok(()));
        }
        Err(err) => {
            if matches!(err, Error::ConnectionLost(_)) {
                *lost = true;
            }
            if failed.is_none() {
                *failed = Some(err.to_string());
            }
            let _ = acks.send(Err(err));
        }
    }
}

/// The first failure, said again.
///
/// [`Error`] is not `Clone`, so the failure is kept as the text it will be
/// shown as plus the one bit that changes what the caller does with it
/// (a lost connection stops a batch, a refused
/// file does not).
fn rebuild(failed: &Option<String>, lost: bool, authority: &str, path: &str) -> Error {
    if lost {
        return Error::ConnectionLost(authority.to_string());
    }
    match failed {
        Some(text) => Error::msg(text.clone()),
        None => Error::msg(format!("{path}: the write failed")),
    }
}

/// One chunk, split across as many `SSH_FXP_WRITE`s as the server's limit
/// needs.
async fn write_window(
    session: &RawSftpSession,
    handle: &str,
    offset: u64,
    data: Vec<u8>,
    max: usize,
    path: &str,
    authority: &str,
) -> std::result::Result<(), Error> {
    let step = max.max(1);
    let mut at = 0usize;
    while at < data.len() {
        let end = at.saturating_add(step).min(data.len());
        let Some(piece) = data.get(at..end) else {
            break;
        };
        session
            .write(
                handle.to_string(),
                offset.saturating_add(at as u64),
                piece.to_vec(),
            )
            .await
            .map_err(|e| attrs::map_error(path, authority, &e))?;
        at = end;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_pool_refuses_rather_than_indexing_into_nothing() {
        let pool = Pool::new(Vec::new());
        assert!(pool.take().is_none());
    }

    #[test]
    fn the_wire_limits_start_at_the_floor_every_server_must_accept() {
        let limits = WireLimits::default();
        assert_eq!(limits.read, SAFE_PAYLOAD);
        assert_eq!(limits.write, SAFE_PAYLOAD);
        assert_eq!(SAFE_PAYLOAD, 32 * 1024);
    }

    #[test]
    fn a_context_that_sees_a_lost_connection_clears_liveness() {
        let live = Arc::new(AtomicBool::new(true));
        let ctx = Context {
            authority: "sftp://t@h:22".to_string(),
            limits: WireLimits::default(),
            depth: 4,
            window: 32 * 1024,
            live: Arc::clone(&live),
        };
        ctx.observe(&Error::NotFound("/a".to_string()));
        assert!(
            live.load(Ordering::Acquire),
            "one missing file is not a dead link"
        );
        ctx.observe(&Error::ConnectionLost("sftp://t@h:22".to_string()));
        assert!(!live.load(Ordering::Acquire));
    }

    #[test]
    fn refusing_a_command_answers_it_rather_than_dropping_it() {
        // A caller parked on `recv` must never wait forever: every variant
        // answers, which is what the exhaustive match in `refuse` guarantees.
        let live = Arc::new(AtomicBool::new(false));
        let ctx = Context {
            authority: "sftp://t@h:22".to_string(),
            limits: WireLimits::default(),
            depth: 4,
            window: 32 * 1024,
            live,
        };
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        refuse(
            Command::List {
                dir: "/srv".to_string(),
                reply: tx,
            },
            &ctx,
        );
        let answer = rx.recv().expect("an answer, not a hang");
        assert!(matches!(answer, Err(Error::ConnectionLost(_))));
    }

    #[test]
    fn a_rebuilt_failure_keeps_the_one_bit_that_changes_what_happens_next() {
        let lost = rebuild(&Some("gone".to_string()), true, "sftp://t@h:22", "/a");
        assert!(
            matches!(lost, Error::ConnectionLost(_)),
            "I14 stops the batch"
        );
        let refused = rebuild(&Some("/a: Permission denied".to_string()), false, "x", "/a");
        assert_eq!(refused.to_string(), "/a: Permission denied");
        let unknown = rebuild(&None, false, "x", "/a");
        assert_eq!(unknown.to_string(), "/a: the write failed");
    }
}
