//! Control connections, leased and returned.
//!
//! An FTP control connection is stateful: it has a working directory, a
//! transfer mode and a login. Sharing one between two concurrent operations
//! would interleave their commands, so each operation leases one for its
//! duration.
//!
//! The invariant a lease keeps: **a leased connection is returned or is
//! dropped, never both, and a dead one is never handed out twice.** Returning
//! is what [`Lease`]'s `Drop` does, which is why no caller has to remember to;
//! and a connection that failed is replaced by a placeholder that answers
//! every command with the same error, so the failure is reported once at the
//! call that caused it rather than again at every later use.
//!
//! Idle connections are said goodbye to rather than dropped, because a server
//! that never sees `QUIT` counts the session against its connection limit
//! until it times out.

use super::session::Session;
use super::*;

/// The logged-in control connections of one [`FtpFs`] (
/// "Connections are pooled per host so that a copy in the background and
/// browsing in the foreground do not fight over one channel").
///
/// FTP carries one command at a time per connection and its data transfer
/// occupies the connection for the whole transfer, so the pool is a pool of
/// whole connections rather than of channels on one - the difference from
/// SFTP that the design names.
pub(super) struct Pool {
    /// `ftp://user@host:port`, for an error that has to name the connection.
    pub(super) authority: String,
    state: Mutex<PoolState>,
    /// Signalled when a connection is returned or the pool is closed.
    ready: Condvar,
}

/// What the pool guards.
pub(super) struct PoolState {
    /// Connections nobody is using.
    pub(super) idle: Vec<Box<dyn Session>>,
    /// How many connections exist at all, idle or leased. Reaching zero is how
    /// a connection is discovered to be gone.
    total: usize,
    /// False once the connection is closed or lost.
    live: bool,
}

impl Pool {
    /// A pool holding the connection that logged in.
    pub(super) fn new(authority: String, first: Box<dyn Session>) -> Arc<Self> {
        Arc::new(Self {
            authority,
            state: Mutex::new(PoolState {
                idle: vec![first],
                total: 1,
                live: true,
            }),
            ready: Condvar::new(),
        })
    }

    /// Take the lock, treating a poisoned mutex as a lock: a panic in another
    /// thread must not turn every later listing into a second failure.
    pub(super) fn lock(&self) -> std::sync::MutexGuard<'_, PoolState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Add another logged-in connection.
    pub(super) fn add(&self, session: Box<dyn Session>) {
        let mut state = self.lock();
        if !state.live {
            return;
        }
        state.total += 1;
        state.idle.push(session);
        drop(state);
        self.ready.notify_one();
    }

    /// Borrow a connection, waiting while every one of them is busy.
    ///
    /// **Blocking.** Waiting for the transfer in front of you to finish is the
    /// honest behaviour of a pool that has run out: this is not a timeout on
    /// an operation, which the design rules out, and it
    /// never interrupts a transfer that is running - it only gives up on
    /// *waiting* for one.
    ///
    /// It gives up because it must: with `remote.pool_size = 1`, a copy whose
    /// source and destination are the same connection holds the only
    /// connection while it asks for a second one, and a wait with no end is
    /// how that becomes a hung file manager instead of a message. The bound is
    /// long enough that no honest transfer reaches it and the caller can
    /// always try again with `Ctrl+R`.
    pub(super) fn checkout(self: &Arc<Self>) -> Result<Lease> {
        let mut state = self.lock();
        let deadline = std::time::Instant::now() + POOL_WAIT;
        loop {
            if !state.live || state.total == 0 {
                return Err(Error::ConnectionLost(self.authority.clone()));
            }
            if let Some(session) = state.idle.pop() {
                return Ok(Lease {
                    pool: Arc::clone(self),
                    session,
                    poisoned: false,
                });
            }
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return Err(Error::msg(format!(
                    "{}: every connection is busy with a transfer",
                    self.authority
                )));
            }
            let (guard, _) = self
                .ready
                .wait_timeout(state, left)
                .unwrap_or_else(PoisonError::into_inner);
            state = guard;
        }
    }

    /// Give a connection back, or forget it when the connection it rode on is
    /// gone. A pool that has forgotten its last connection is a lost
    /// connection, which is what `is_live` then reports.
    fn give_back(&self, session: Option<Box<dyn Session>>) {
        let mut state = self.lock();
        match session {
            Some(session) if state.live => state.idle.push(session),
            Some(_) => state.total = state.total.saturating_sub(1),
            None => state.total = state.total.saturating_sub(1),
        }
        if state.total == 0 {
            state.live = false;
        }
        drop(state);
        self.ready.notify_one();
    }

    pub(super) fn is_live(&self) -> bool {
        self.lock().live
    }

    /// Close every idle connection and refuse every future checkout.
    /// Idempotent; a connection that is leased out closes when its lease ends.
    ///
    /// Returns as soon as the pool is marked dead: the farewells are said on a
    /// thread of their own. This is reached straight from `dispatch` -
    /// `Ctrl+F` and Y, closing a tab, navigating off a connected panel - and
    /// the design forbids I/O there. A `QUIT` is a round trip,
    /// there are `remote.pool_size` of them, and the read timeout that bounded
    /// the greeting was deliberately dropped after login, so saying them here
    /// froze the whole TUI - keys, redraws and the signal arms of the event
    /// loop - against a server that had stopped answering.
    pub(super) fn close(&self) {
        let mut state = self.lock();
        state.live = false;
        let idle = std::mem::take(&mut state.idle);
        state.total = state.total.saturating_sub(idle.len());
        drop(state);
        self.ready.notify_all();
        farewell(idle);
    }
}

/// How long a farewell `QUIT` waits for the server's `221` before its socket
/// is dropped anyway.
///
/// The reply is a courtesy - it frees the login slot at once rather than at
/// the server's idle timeout - so it is worth a few seconds and not worth a
/// thread that lives for ever.
const QUIT_TIMEOUT: Duration = Duration::from_secs(5);

/// Say `QUIT` to connections nobody is using any more, off the caller's
/// thread.
///
/// A plain `std::thread` rather than `spawn_blocking`, because [`Pool::close`]
/// is reached from `Drop` as well as from the event loop and a `Drop` cannot
/// count on a tokio runtime being current. Nothing waits for the thread: the
/// pool is already dead to every caller, and a process that exits first costs
/// the server an idle timeout and nothing else.
pub(super) fn farewell(idle: Vec<Box<dyn Session>>) {
    if idle.is_empty() {
        return;
    }
    // A machine that cannot spawn a thread drops the sockets instead, which
    // the server reads as a disconnect. There is nothing better to do here and
    // nothing worth saying to the user about it, so the handle is dropped:
    // dropping it is what detaches the thread.
    let _ = std::thread::Builder::new()
        .name("ftp-quit".to_string())
        .spawn(move || {
            for mut session in idle {
                session.set_read_timeout(Some(QUIT_TIMEOUT));
                let _ = session.quit();
            }
        });
}

/// How long a command waits for a busy pool before it says so
/// ([`Pool::checkout`] explains why the wait ends at all).
const POOL_WAIT: Duration = Duration::from_secs(300);

/// One connection, borrowed. Returned to the pool when it drops, unless it was
/// poisoned - a connection that failed at the protocol level is closed rather
/// than handed to the next caller mid-sentence.
pub(super) struct Lease {
    pool: Arc<Pool>,
    pub(super) session: Box<dyn Session>,
    pub(super) poisoned: bool,
}

impl Lease {
    /// The borrowed connection.
    pub(super) fn session(&mut self) -> &mut dyn Session {
        self.session.as_mut()
    }

    /// Turn a suppaftp error into this crate's, poisoning the connection when
    /// the failure was the connection rather than the request.
    pub(super) fn fail(&mut self, err: FtpError, authority: &str, what: &'static str) -> Error {
        if fatal(&err) {
            self.poisoned = true;
        }
        translate(err, authority, what)
    }
}

impl Drop for Lease {
    /// [`DeadSession`] is what makes taking the connection out of a value that
    /// is being dropped possible without an `Option` and without an `unwrap`:
    /// the lease is left holding a connection that answers "gone" to
    /// everything, and nobody can reach it anyway.
    fn drop(&mut self) {
        let session = std::mem::replace(&mut self.session, Box::new(DeadSession));
        if self.poisoned {
            self.pool.give_back(None);
            drop(session);
        } else {
            self.pool.give_back(Some(session));
        }
    }
}

/// The connection a lease has already given up.
///
/// Every method answers "the connection is gone", which is what lets
/// [`Lease::drop`] move the real connection out of a value it only has by
/// `&mut` - without an `Option` and without a panic path.
struct DeadSession;

impl Session for DeadSession {
    fn login(&mut self, _user: &str, _password: &str) -> FtpResult<()> {
        Err(dead())
    }
    fn binary(&mut self) -> FtpResult<()> {
        Err(dead())
    }
    fn set_read_timeout(&mut self, _timeout: Option<Duration>) {}
    fn pwd(&mut self) -> FtpResult<String> {
        Err(dead())
    }
    fn feat(&mut self) -> FtpResult<Features> {
        Err(dead())
    }
    fn mlsd(&mut self, _dir: &str) -> FtpResult<Vec<String>> {
        Err(dead())
    }
    fn list(&mut self, _dir: &str) -> FtpResult<Vec<String>> {
        Err(dead())
    }
    fn mlst(&mut self, _path: &str) -> FtpResult<String> {
        Err(dead())
    }
    fn size(&mut self, _path: &str) -> FtpResult<u64> {
        Err(dead())
    }
    fn mdtm(&mut self, _path: &str) -> FtpResult<Option<SystemTime>> {
        Err(dead())
    }
    fn mkdir(&mut self, _path: &str) -> FtpResult<()> {
        Err(dead())
    }
    fn rm(&mut self, _path: &str) -> FtpResult<()> {
        Err(dead())
    }
    fn rmdir(&mut self, _path: &str) -> FtpResult<()> {
        Err(dead())
    }
    fn rename(&mut self, _from: &str, _to: &str) -> FtpResult<()> {
        Err(dead())
    }
    fn retr_start(&mut self, _path: &str) -> FtpResult<Box<dyn Read + Send>> {
        Err(dead())
    }
    fn retr_finish(&mut self, _data: Box<dyn Read + Send>) -> FtpResult<()> {
        Err(dead())
    }
    fn retr_abort(&mut self, _data: Box<dyn Read + Send>) -> FtpResult<()> {
        Err(dead())
    }
    fn stor_start(&mut self, _path: &str) -> FtpResult<Box<dyn Write + Send>> {
        Err(dead())
    }
    fn stor_finish(&mut self, _data: Box<dyn Write + Send>) -> FtpResult<()> {
        Err(dead())
    }
    fn quit(&mut self) -> FtpResult<()> {
        Ok(())
    }
}

/// The error a connection that is gone answers with.
pub(super) fn dead() -> FtpError {
    FtpError::ConnectionError(std::io::Error::new(
        std::io::ErrorKind::NotConnected,
        "the connection is closed",
    ))
}
