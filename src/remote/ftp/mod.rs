//! FTP and FTPS, on `suppaftp`.
//!
//! the design asks for one line of behaviour: "Explicit and implicit TLS,
//! passive by default". The rest of this module is what that costs once the
//! [`RemoteTransport`] contract has to be met by a protocol that is not a
//! filesystem.
//!
//! # Nothing here assumes POSIX
//!
//! the design both say the trait must not bake in POSIX semantics,
//! and this backend is the reason they say it. An [`Entry`] produced here has
//! `mode: 0`, `uid: 0` and `gid: 0`, and is never [`EntryKind::Symlink`]:
//!
//! * FTP has no mode bits to report. An `MLSD` `perm` fact is a string of
//!   *capabilities the logged-in user has* (`perm=fle`), not a permission set,
//!   and it says nothing about anybody else. A `LIST` line that looks like
//!   `ls -l` output is a rendering chosen by the server, and servers render
//!   things they do not have.
//! * FTP has no `READLINK`. A `LIST` line may show `name -> target` and an
//!   `MLSD` line may say `type=OS.unix=slink`, but there is no operation that
//!   asks where a link points, no way to create one, and no way to know
//!   whether the target is a directory. Reporting [`EntryKind::Symlink`] would
//!   promise the copy engine an answer to `read_link` that this backend cannot
//!   give (the design copies a link as a link), so a link is reported as
//!   [`EntryKind::Other`] and [`FtpFs::read_link`] refuses.
//!
//! the design is the whole of that mapping and I13 tests it.
//!
//! # The listing dialects
//!
//! `MLSD` (RFC 3659) is machine-readable, is UTC, and is what this backend
//! uses whenever `FEAT` says the server has it. `LIST` has no specification at
//! all - its output is whatever the server felt like printing - so
//! [`parse_list`] recognises the two dialects that between them cover the
//! servers in use, and says `None` to anything else:
//!
//! * the Unix `ls -l` dialect, in both its `Nov  5 13:46` and its
//!   `2024-01-10 13:46` spellings (vsftpd, ProFTPD, pure-ftpd, and every
//!   server that shells out to something `ls`-shaped),
//! * the DOS dialect (IIS in MS-DOS listing mode).
//!
//! EPLF, VMS and NetWare listings are **not** parsed. What happens then is
//! written into the contract rather than left to chance: [`parse_list`]
//! returns `None` for a line it does not recognise, [`FtpFs::list`] skips such
//! a line, and a directory in which *no* candidate line was recognised is an
//! `Err` naming the directory and the dialects that were tried - never an
//! empty panel, which would be a lie about a directory that has files in it.
//!
//! # Blocking
//!
//! **Every method here blocks. Call from the blocking pool only**.
//! There is no actor and no runtime: suppaftp's
//! synchronous `FtpStream` is used on the calling thread, from a pool guarded
//! by a `Mutex` and a `Condvar`, which is what the pooling means
//! for a protocol that carries one command at a time per connection.
//!
//! # Secrets
//!
//! A password reaches exactly one call here, [`Session::login`], and is never
//! stored in [`FtpFs`], never logged (suppaftp is built with `no-log`, which
//! compiles out its `CC OUT: PASS ...` trace), and never formatted into an
//! error.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use chrono::Datelike;
use suppaftp::rustls::pki_types::CertificateDer;
use suppaftp::rustls::{ClientConfig, RootCertStore};
use suppaftp::types::{Features, FileType, FtpError, FtpResult, Mode};
use suppaftp::{FtpStream, ImplFtpStream, RustlsConnector, RustlsFtpStream, TlsStream};

use crate::config::RemoteConfig;
use crate::dialog::SecretAnswer;
use crate::error::{Error, Result};
use crate::remote::auth::{
    ANONYMOUS_PASSWORD, AuthPlan, AuthSequence, MAX_ASKS, Method, Outcome, SecretKind, is_anonymous,
};
use crate::remote::keyring::SecretStore;
use crate::remote::secret::Secret;
use crate::remote::transport::RemoteTransport;
use crate::remote::{Protocol, Target};
use crate::vfs::{Capabilities, Entry, EntryKind, ReadSeek};

pub mod parse;
pub mod pool;
pub mod session;
pub mod tls;

use parse::{entries_from_list, entries_from_mlsd, parse_mlst};
use pool::{Lease, Pool};
use session::{ServerFacts, Session};
use tls::tls_config;

/// The user name an anonymous login sends (the design's
/// `ftp://anonymous@ftp.example.org`).
///
/// The password it sends is [`ANONYMOUS_PASSWORD`], and the rule for which
/// users log in this way is [`is_anonymous`]: both live in
/// `crate::remote::auth`, because the plan that has no methods in it and the
/// login that needs none are the same decision.
pub const ANONYMOUS_USER: &str = "anonymous";

/// the `FtpFs`: suppaftp behind [`RemoteTransport`].
///
///
/// **No actor.** suppaftp's synchronous API is used on the calling blocking
/// thread, from a pool of `remote.pool_size` streams guarded by a mutex.
pub struct FtpFs {
    /// Where this connection points. Carries no secret, which is what makes it
    /// safe in a header and in an error.
    target: Target,
    /// The directory the server put us in, or the one that was asked for.
    start: String,
    /// The logged-in control connections (the pooling).
    pool: Arc<Pool>,
    /// What `FEAT` said the server can do, narrowed at run time when a command
    /// it advertised turns out not to work.
    facts: ServerFacts,
}

impl FtpFs {
    /// Connect and log in.
    ///
    /// **Blocking**: it runs under `spawn_blocking` from the connect task, so
    /// the connect task itself stays async and can still answer a dialog.
    ///
    ///
    /// `hooks` is how it asks for a password: FTP's plan has no agent and no
    /// key, so the only question it can ask is [`SecretKind::Password`], and a
    /// `None` answer is a cancelled connect.
    ///
    /// The whole pool is opened here, while the password is in hand, because
    /// [`FtpFs`] deliberately does **not** keep one: the design lists the
    /// five places a secret may be and a backend field is not one of them. A
    /// server that refuses the extra logins is not an error - the pool is
    /// simply smaller, down to the one connection that must work.
    pub fn connect(
        target: Target,
        plan: AuthPlan,
        password: Option<Secret>,
        config: &RemoteConfig,
        store: Arc<dyn SecretStore>,
        hooks: BlockingHooks,
    ) -> Result<Arc<Self>> {
        let timeout = config.connect_timeout.duration();
        let addr = resolve(&target, timeout)?;
        let mut session = dial(&target, addr, timeout)?;

        // the order, walked as the state machine so that the order is
        // testable with no server.
        let credential = authenticate(
            session.as_mut(),
            &target,
            plan,
            password,
            store.as_ref(),
            &hooks,
        )?;

        // Binary, always. the design copies bytes; an ASCII-mode transfer
        // rewrites line endings and makes the byte count a lie.
        session
            .binary()
            .map_err(|err| translate(err, &target.authority(), "TYPE I"))?;

        let facts = ServerFacts::probe(session.as_mut());

        let start = match target.dir.clone() {
            Some(dir) => normalize(&dir),
            // no directory on the connect line means "wherever
            // the server puts us", which FTP answers with PWD.
            None => match session.pwd() {
                Ok(dir) => normalize(&dir),
                // A server that will not answer PWD still has a root.
                Err(_) => "/".to_string(),
            },
        };

        // The connect timeout bounded the greeting; a transfer must not be
        // bounded by anything but the user's Esc (the design: "A
        // per-operation timeout on a filesystem is how a slow but working
        // link turns into a corrupt copy").
        session.set_read_timeout(None);

        let pool = Pool::new(target.authority(), session);
        let wanted = config.pool_size.max(1);
        for _ in 1..wanted {
            match open_extra(&target, addr, timeout, &credential) {
                Ok(extra) => pool.add(extra),
                // the design wants a pool; a server with a per-user
                // connection limit gets one of whatever size it allows, and
                // that is a smaller pool rather than a failed connect.
                Err(_) => break,
            }
        }

        Ok(Arc::new(Self {
            target,
            start,
            pool,
            facts,
        }))
    }

    /// The directory a panel opens on: `remote_dir` when the host book named
    /// one, and the server's own login directory otherwise.
    ///
    /// Inherent rather than on [`RemoteTransport`], because it is answered
    /// once at connect time and never again; see the report on
    /// the design, which has no method for it.
    pub fn start_dir(&self) -> &str {
        &self.start
    }

    /// Where this connection points (the header).
    pub fn target(&self) -> &Target {
        &self.target
    }

    /// Run one command on a pooled connection.
    ///
    /// **Blocking.** Call from the blocking pool only. It waits for a free
    /// connection when every one of them is busy, which is what makes
    /// browsing during a copy work at all when the pool has more than one
    /// connection in it, and what makes it wait rather than fail when it does
    /// not.
    fn command<T>(
        &self,
        what: &'static str,
        run: impl FnOnce(&mut dyn Session) -> FtpResult<T>,
    ) -> Result<T> {
        let mut lease = self.pool.checkout()?;
        let outcome = run(lease.session());
        match outcome {
            Ok(value) => Ok(value),
            Err(err) => Err(lease.fail(err, &self.pool.authority, what)),
        }
    }
}

impl RemoteTransport for FtpFs {
    fn protocol(&self) -> Protocol {
        self.target.protocol
    }

    fn capabilities(&self) -> Capabilities {
        // Never I/O: the answer was fixed when the connection was established,
        // and `App::apply_vfs_event` asks for it on the event loop.
        //
        Capabilities::FTP
    }

    /// **Blocking.** Call from the blocking pool only.
    fn list(&self, dir: &str) -> Result<Vec<Entry>> {
        let dir = normalize(dir);
        check_path(&dir)?;
        if self.facts.mlsd.load(Ordering::Relaxed) {
            let outcome = self
                .command("MLSD", |s| s.mlsd(&dir))
                .and_then(|lines| entries_from_mlsd(&lines));
            match outcome {
                Ok(entries) => return Ok(entries),
                // Advertised and then refused, or advertised and then answered
                // in something that is not MLSD at all. Narrow what we believe
                // about this server and fall through to LIST, once.
                Err(err) if retry_without(&err) => {
                    self.facts.mlsd.store(false, Ordering::Relaxed);
                }
                Err(err) => return Err(err),
            }
        }
        let lines = self.command("LIST", |s| s.list(&dir))?;
        entries_from_list(&dir, &lines)
    }

    /// **Blocking.** Call from the blocking pool only.
    fn stat(&self, path: &str) -> Result<Entry> {
        let path = normalize(path);
        check_path(&path)?;
        let Some((parent, name)) = split_path(&path) else {
            // The root of the connection is a directory by definition, and
            // there is no listing that contains it.
            return Ok(Entry::dir("/"));
        };

        if self.facts.mlst.load(Ordering::Relaxed) {
            match self.command("MLST", |s| s.mlst(&path)) {
                Ok(line) => {
                    if let Some(entry) = parse_mlst(&line) {
                        return Ok(entry);
                    }
                    self.facts.mlst.store(false, Ordering::Relaxed);
                }
                Err(err) => {
                    if !retry_without(&err) {
                        return Err(err);
                    }
                    self.facts.mlst.store(false, Ordering::Relaxed);
                }
            }
        }

        // SIZE and MDTM answer for a file and, on most servers, refuse for a
        // directory - which is why a refusal falls through to the parent's
        // listing rather than being reported.
        if self.facts.size.load(Ordering::Relaxed)
            && let Ok(size) = self.command("SIZE", |s| s.size(&path))
        {
            let mtime = if self.facts.mdtm.load(Ordering::Relaxed) {
                self.command("MDTM", |s| s.mdtm(&path)).ok().flatten()
            } else {
                None
            };
            let mut entry = Entry::file(name);
            entry.size = size;
            entry.mtime = mtime;
            return Ok(entry);
        }

        let siblings = self.list(&parent)?;
        siblings
            .into_iter()
            .find(|entry| entry.name == name)
            .ok_or_else(|| Error::NotFound(format!("{}{}", self.target.authority(), path)))
    }

    /// **Blocking.** Call from the blocking pool only.
    fn read_link(&self, path: &str) -> Result<String> {
        // FTP has no operation that asks where a link points, and this backend
        // never reports one (the module documentation says why).
        let _ = path;
        Err(Error::Unsupported("reading a symbolic link over FTP"))
    }

    /// **Blocking.** Call from the blocking pool only.
    fn open_read(&self, path: &str) -> Result<Box<dyn Read + Send>> {
        let path = normalize(path);
        check_path(&path)?;
        let mut lease = self.pool.checkout()?;
        let data = match lease.session().retr_start(&path) {
            Ok(data) => data,
            Err(err) => return Err(lease.fail(err, &self.pool.authority, "RETR")),
        };
        Ok(Box::new(FtpReader {
            lease: Some(lease),
            data: Some(data),
            authority: self.pool.authority.clone(),
            done: false,
        }))
    }

    /// **Blocking.** Call from the blocking pool only.
    fn open_seek(&self, path: &str) -> Result<Box<dyn ReadSeek + Send>> {
        // `Capabilities::FTP.seekable` is false and the two must agree:
        // REST is optional, servers disagree
        // about it, and the viewer's forward-only mode is correct rather than
        // merely slower.
        let _ = path;
        Err(Error::Unsupported("random-access reading over FTP"))
    }

    /// **Blocking.** Call from the blocking pool only.
    fn open_write(&self, path: &str) -> Result<Box<dyn Write + Send>> {
        let path = normalize(path);
        check_path(&path)?;
        let mut lease = self.pool.checkout()?;
        let data = match lease.session().stor_start(&path) {
            Ok(data) => data,
            Err(err) => return Err(lease.fail(err, &self.pool.authority, "STOR")),
        };
        Ok(Box::new(FtpWriter {
            lease: Some(lease),
            data: Some(data),
            authority: self.pool.authority.clone(),
            done: false,
        }))
    }

    /// **Blocking.** Call from the blocking pool only.
    fn create_dir(&self, path: &str) -> Result<()> {
        let path = normalize(path);
        check_path(&path)?;
        self.command("MKD", |s| s.mkdir(&path))
    }

    /// **Blocking.** Call from the blocking pool only.
    fn remove_file(&self, path: &str) -> Result<()> {
        let path = normalize(path);
        check_path(&path)?;
        self.command("DELE", |s| s.rm(&path))
    }

    /// **Blocking.** Call from the blocking pool only.
    fn remove_dir(&self, path: &str) -> Result<()> {
        let path = normalize(path);
        check_path(&path)?;
        self.command("RMD", |s| s.rmdir(&path))
    }

    /// **Blocking.** Call from the blocking pool only.
    fn rename(&self, from: &str, to: &str) -> Result<()> {
        let from = normalize(from);
        let to = normalize(to);
        check_path(&from)?;
        check_path(&to)?;
        // RNFR then RNTO is one server-side operation, which is what
        // `Capabilities::FTP.atomic_rename` promises. It may still fail when
        // the target exists, which `ops::copy` already degrades from.
        self.command("RNFR/RNTO", |s| s.rename(&from, &to))
    }

    fn is_live(&self) -> bool {
        // Never I/O: it reads the flag the pool keeps.
        self.pool.is_live()
    }

    fn close(&self) {
        self.pool.close();
    }
}

impl std::fmt::Debug for FtpFs {
    /// Target and liveness. There is no session, no socket and no credential
    /// in here, because a `Debug` is one `{:?}` away from a log line.
    ///
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FtpFs")
            .field("target", &self.target)
            .field("live", &self.is_live())
            .finish()
    }
}

impl Drop for FtpFs {
    /// Say QUIT rather than dropping sockets on the floor, so the server frees
    /// the login slot at once. The twin of `SftpFs`'s.
    fn drop(&mut self) {
        self.pool.close();
    }
}

/// How the blocking connect path asks the user a question
/// (the design names the type and does not define it; the
/// report records that).
///
/// FTP's only question is a password, so this is one closure. The event loop
/// builds it out of a `RemoteEvent::Secret` and a `oneshot`; a test builds it
/// out of a constant. A `None` answer is a refusal, which is how `Esc`
/// cancels a connect with no second path.
pub struct BlockingHooks {
    /// Ask for a secret. `None` is a refusal.
    ask: Box<dyn Fn(SecretKind, bool) -> Option<SecretAnswer> + Send + Sync>,
}

impl BlockingHooks {
    /// Hooks that ask through `ask`.
    pub fn new(
        ask: impl Fn(SecretKind, bool) -> Option<SecretAnswer> + Send + Sync + 'static,
    ) -> Self {
        Self { ask: Box::new(ask) }
    }

    /// Hooks that refuse every question, for a connect that has nowhere to ask
    /// - a test, or any context with no UI.
    pub fn none() -> Self {
        Self::new(|_, _| None)
    }

    /// Put the question on the screen and wait for the answer.
    pub fn ask_secret(&self, kind: SecretKind, offer_keyring: bool) -> Option<SecretAnswer> {
        (self.ask)(kind, offer_keyring)
    }
}

impl std::fmt::Debug for BlockingHooks {
    /// The closure can produce a secret, so nothing about it is printed.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BlockingHooks")
    }
}

// ------------------------------------------------------------ connecting ----

/// What logged in, kept only long enough to open the rest of the pool.
///
/// It is a local of [`FtpFs::connect`] and of nothing else: it is never a
/// field of [`FtpFs`].
struct Credential {
    user: String,
    password: Secret,
}

/// Resolve the host, honouring the connect timeout as far as the standard
/// library allows.
///
/// `ToSocketAddrs` has no timeout and there is no way to give it one without a
/// second thread, so DNS is bounded by the resolver's own timeout and not by
/// `remote.connect_timeout`; everything after it is bounded here.
///
fn resolve(target: &Target, _timeout: Duration) -> Result<SocketAddr> {
    (target.hostname(), target.port)
        .to_socket_addrs()
        .map_err(|err| Error::msg(format!("{}: {err}", target.authority())))?
        .next()
        .ok_or_else(|| {
            Error::msg(format!(
                "{}: the host name did not resolve",
                target.authority()
            ))
        })
}

/// One logged-out control connection, TLS negotiated where the protocol asks
/// for it ("Explicit and implicit TLS, passive by default").
fn dial(target: &Target, addr: SocketAddr, timeout: Duration) -> Result<Box<dyn Session>> {
    let authority = target.authority();
    match target.protocol {
        Protocol::Sftp
        | Protocol::Smb
        | Protocol::Dav
        | Protocol::Davs
        | Protocol::S3
        | Protocol::S3Http => Err(Error::msg(format!(
            "{authority}: {} is not this backend's protocol",
            target.protocol
        ))),
        Protocol::Ftp => {
            let stream = greet(addr, timeout, &authority)?;
            let mut session = FtpStream::connect_with_stream(stream)
                .map_err(|err| translate(err, &authority, "the greeting"))?;
            passive(&mut session, addr);
            Ok(Box::new(session))
        }
        Protocol::Ftps => {
            let stream = greet(addr, timeout, &authority)?;
            let session = RustlsFtpStream::connect_with_stream(stream)
                .map_err(|err| translate(err, &authority, "the greeting"))?;
            // Explicit TLS: AUTH TLS on the control port, then PBSZ 0 and
            // PROT P, which is what `into_secure` performs.
            let mut session = session
                .into_secure(RustlsConnector::from(tls_config()?), target.hostname())
                .map_err(|err| translate(err, &authority, "AUTH TLS"))?;
            passive(&mut session, addr);
            Ok(Box::new(session))
        }
        Protocol::FtpsImplicit => {
            // Implicit TLS: the socket is TLS from its first byte, so the
            // greeting is read inside the tunnel and there is no plaintext
            // moment. suppaftp offers no way to hand it an already-wrapped
            // socket, so this is its own constructor - and it dials for
            // itself, which is why `connect_timeout` cannot reach the TCP
            // connect on this one path (the report says so).
            let mut session = RustlsFtpStream::connect_secure_implicit(
                addr,
                RustlsConnector::from(tls_config()?),
                target.hostname(),
            )
            .map_err(|err| translate(err, &authority, "the implicit-TLS greeting"))?;
            passive(&mut session, addr);
            Ok(Box::new(session))
        }
    }
}

/// Connect the socket and arm the read timeout that bounds the greeting.
///
/// `remote.connect_timeout` "wraps DNS, TCP, the SSH handshake and the FTP
/// greeting" and stops there; the timeout is
/// cleared once the connection is logged in.
fn greet(addr: SocketAddr, timeout: Duration, authority: &str) -> Result<TcpStream> {
    let stream = TcpStream::connect_timeout(&addr, timeout)
        .map_err(|err| Error::msg(format!("{authority}: {err}")))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|err| Error::msg(format!("{authority}: {err}")))?;
    Ok(stream)
}

/// "passive by default", and there is no configuration key that
/// turns it off. `EPSV` rather than `PASV` over IPv6, because `PASV` can only
/// express a four-byte address.
fn passive<T: TlsStream>(session: &mut ImplFtpStream<T>, addr: SocketAddr) {
    session.set_mode(if addr.is_ipv6() {
        Mode::ExtendedPassive
    } else {
        Mode::Passive
    });
}

/// A second, third and fourth connection for the pool, logged in the same way
/// the first one was (the pooling).
fn open_extra(
    target: &Target,
    addr: SocketAddr,
    timeout: Duration,
    credential: &Credential,
) -> Result<Box<dyn Session>> {
    let mut session = dial(target, addr, timeout)?;
    let authority = target.authority();
    let password = credential
        .password
        .expose_str()
        .ok_or_else(|| Error::msg(format!("{authority}: the password is not valid UTF-8")))?;
    session
        .login(&credential.user, password)
        .map_err(|err| translate(err, &authority, "PASS"))?;
    session
        .binary()
        .map_err(|err| translate(err, &authority, "TYPE I"))?;
    session.set_read_timeout(None);
    Ok(session)
}

/// Walk the order for a protocol that has neither an agent nor a
/// key.
///
/// The plan is `[Password]`, or `[Stored, Password]` for a host that opted
/// into the keyring; `Agent` and `Key` are recorded [`Outcome::Unavailable`],
/// which is not a failure and is shown only if nothing worked. An anonymous
/// user skips the whole thing, which is what
/// `ftp://anonymous@ftp.example.org` has to mean.
fn authenticate(
    session: &mut dyn Session,
    target: &Target,
    plan: AuthPlan,
    typed: Option<Secret>,
    store: &dyn SecretStore,
    hooks: &BlockingHooks,
) -> Result<Credential> {
    let authority = target.authority();
    if is_anonymous(&target.user) {
        session
            .login(ANONYMOUS_USER, ANONYMOUS_PASSWORD)
            .map_err(|err| translate(err, &authority, "PASS"))?;
        return Ok(Credential {
            user: ANONYMOUS_USER.to_string(),
            password: Secret::from_str(ANONYMOUS_PASSWORD),
        });
    }

    // The plan is the record of the per-host opt-in: `Stored` is in it only
    // for a host whose `auth` is `keyring`, which is exactly when the prompt
    // may offer to remember the answer.
    let opted_in = plan.methods().iter().any(|m| matches!(m, Method::Stored));
    let mut typed = typed;
    let mut sequence = AuthSequence::new(plan);
    while let Some(method) = sequence.peek().cloned() {
        // Exhaustive, on this crate's enum: the design has four methods and
        // two of them are SSH's.
        let (secret, remember) = match method {
            Method::Agent => {
                sequence.record(Outcome::Unavailable(
                    "FTP has no agent authentication".to_string(),
                ));
                continue;
            }
            Method::Key { .. } => {
                sequence.record(Outcome::Unavailable(
                    "FTP has no key authentication".to_string(),
                ));
                continue;
            }
            Method::Stored => match store.get(&target.keyring_account()) {
                Ok(Some(secret)) => (secret, false),
                Ok(None) => {
                    sequence.record(Outcome::Unavailable(
                        "there is no password in the keyring for this host".to_string(),
                    ));
                    continue;
                }
                Err(err) => {
                    sequence.record(Outcome::Unavailable(err.to_string()));
                    continue;
                }
            },
            Method::Password => match typed.take() {
                // A password typed on the connect line is used for this
                // connection and nothing else.
                Some(secret) => (secret, false),
                None => {
                    if sequence.asks() >= MAX_ASKS {
                        sequence
                            .record(Outcome::Unavailable("no password was accepted".to_string()));
                        continue;
                    }
                    let kind = SecretKind::Password {
                        authority: authority.clone(),
                    };
                    // The checkbox is offered only where the host opted in and
                    // a keyring exists; where it opted in and there is none,
                    // the dialog says so instead.
                    let offer = opted_in && store.available();
                    match hooks.ask_secret(kind, offer) {
                        Some(answer) => (answer.secret, answer.remember),
                        None => {
                            return Err(Error::msg(format!(
                                "{authority}: the connection was cancelled"
                            )));
                        }
                    }
                }
            },
        };

        let Some(password) = secret.expose_str() else {
            sequence.record(Outcome::Unavailable(
                "the password is not valid UTF-8".to_string(),
            ));
            continue;
        };
        match session.login(&target.user, password) {
            Ok(()) => {
                if remember {
                    // Only ever after the server has accepted it, so a typo is
                    // not what gets stored.
                    let _ = store.set(&target.keyring_account(), &secret);
                }
                sequence.record(Outcome::Accepted);
                return Ok(Credential {
                    user: target.user.clone(),
                    password: secret,
                });
            }
            Err(err) if rejected(&err) => {
                // Ask again on the same method: a wrong password is
                // re-askable and must not fall through to the next method.
                //
                if matches!(method, Method::Password) && sequence.asks() < MAX_ASKS {
                    sequence.record(Outcome::Needs(SecretKind::Password {
                        authority: authority.clone(),
                    }));
                } else {
                    sequence.record(Outcome::Rejected);
                }
            }
            Err(err) => return Err(translate(err, &authority, "PASS")),
        }
    }

    Err(Error::msg(sequence.failure_message(target)))
}

/// Whether the server said "wrong credentials" as opposed to "the connection
/// is broken": the first is another attempt, the second is the end.
fn rejected(err: &FtpError) -> bool {
    match err {
        FtpError::UnexpectedResponse(response) => {
            matches!(response.status.code(), 430 | 530 | 332 | 421)
        }
        _ => false,
    }
}

// ------------------------------------------------------- reader, writer -----

/// A `RETR` in progress, holding the connection it rides on.
///
/// The lease is released when the transfer ends, whether that is the end of
/// the file, an error, or the reader being dropped early - which is what stops
/// a cancelled `F5` from leaking a connection out of the pool.
struct FtpReader {
    lease: Option<Lease>,
    data: Option<Box<dyn Read + Send>>,
    authority: String,
    done: bool,
}

impl FtpReader {
    /// The server's word for how the transfer ended, which is the difference
    /// between a complete file and a truncated one.
    fn finish(&mut self) -> std::io::Result<()> {
        self.done = true;
        let (Some(mut lease), Some(data)) = (self.lease.take(), self.data.take()) else {
            return Ok(());
        };
        let outcome = lease.session().retr_finish(data);
        match outcome {
            Ok(()) => Ok(()),
            Err(err) => {
                if fatal(&err) {
                    lease.poisoned = true;
                }
                Err(as_io(err, &self.authority))
            }
        }
    }
}

impl Read for FtpReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.done {
            return Ok(0);
        }
        let Some(data) = self.data.as_mut() else {
            return Ok(0);
        };
        match data.read(buf) {
            Ok(0) => self.finish().map(|()| 0),
            Ok(n) => Ok(n),
            Err(err) => {
                if let Some(lease) = self.lease.as_mut() {
                    lease.poisoned = true;
                }
                self.done = true;
                Err(err)
            }
        }
    }
}

impl Drop for FtpReader {
    /// A reader dropped before the end of the file is a cancelled transfer, so
    /// `ABOR` rather than waiting for the rest of a file nobody wants.
    fn drop(&mut self) {
        if self.done {
            return;
        }
        let (Some(mut lease), Some(data)) = (self.lease.take(), self.data.take()) else {
            return;
        };
        // A refused ABOR leaves the command channel in a state nobody can
        // predict, whatever the reason was, so the connection does not go back
        // into the pool. A successful one leaves it at the start of the next
        // reply, which is exactly what pooling it needs.
        if lease.session().retr_abort(data).is_err() {
            lease.poisoned = true;
        }
    }
}

/// A `STOR` in progress, holding the connection it rides on.
///
/// `flush` is the commit: it closes the data
/// connection and reads the server's verdict, and that verdict is `flush`'s
/// return value. `ops::copy::copy_one_via_vfs` already calls `flush()?` and
/// treats it as the commit.
struct FtpWriter {
    lease: Option<Lease>,
    data: Option<Box<dyn Write + Send>>,
    authority: String,
    done: bool,
}

impl FtpWriter {
    /// Close the data connection and read the server's verdict.
    fn commit(&mut self) -> std::io::Result<()> {
        if self.done {
            return Ok(());
        }
        self.done = true;
        let (Some(mut lease), Some(mut data)) = (self.lease.take(), self.data.take()) else {
            return Ok(());
        };
        data.flush()?;
        match lease.session().stor_finish(data) {
            Ok(()) => Ok(()),
            Err(err) => {
                if fatal(&err) {
                    lease.poisoned = true;
                }
                Err(as_io(err, &self.authority))
            }
        }
    }
}

impl Write for FtpWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let Some(data) = self.data.as_mut() else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "the transfer has already finished",
            ));
        };
        match data.write(buf) {
            Ok(n) => Ok(n),
            Err(err) => {
                if let Some(lease) = self.lease.as_mut() {
                    lease.poisoned = true;
                }
                Err(err)
            }
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.commit()
    }
}

impl Drop for FtpWriter {
    /// A writer dropped without a `flush` is a cancelled copy: the data
    /// connection still has to be closed and the response still has to be
    /// read, or the next command on this connection reads the wrong reply.
    fn drop(&mut self) {
        let _ = self.commit();
    }
}

// ---------------------------------------------------------------- paths -----

/// An absolute, `/`-separated remote path, with the trailing slash off
/// everything but the root.
fn normalize(path: &str) -> String {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return "/".to_string();
    }
    let with_root = if trimmed.starts_with('/') {
        trimmed.to_string()
    } else {
        format!("/{trimmed}")
    };
    let cut = with_root.trim_end_matches('/');
    if cut.is_empty() {
        "/".to_string()
    } else {
        cut.to_string()
    }
}

/// Refuse a path that would smuggle a second command onto the control channel.
///
/// An FTP command is one line, so a `\r` or a `\n` inside a file name is a
/// second command to the server. suppaftp checks this too and this backend
/// does not rely on it: a refusal that names the path is a better answer than
/// somebody else's `InvalidInput`, and the check is one line.
fn check_path(path: &str) -> Result<()> {
    if path.contains(['\r', '\n']) {
        return Err(Error::InvalidPath(format!(
            "{path:?} contains a line break, which FTP cannot address"
        )));
    }
    Ok(())
}

/// A path's parent and file name, or `None` at the root.
fn split_path(path: &str) -> Option<(String, String)> {
    let (head, name) = path.rsplit_once('/')?;
    if name.is_empty() {
        return None;
    }
    let parent = if head.is_empty() {
        "/".to_string()
    } else {
        head.to_string()
    };
    Some((parent, name.to_string()))
}

// --------------------------------------------------------------- errors -----

/// Whether a failure was the connection rather than the request.
///
/// A refused command leaves a perfectly good connection behind and must not
/// cost the pool one of its members; a broken socket or a reply nobody can
/// parse leaves a connection that cannot be trusted to be at the start of the
/// next reply.
fn fatal(err: &FtpError) -> bool {
    match err {
        FtpError::ConnectionError(_) | FtpError::SecureError(_) | FtpError::BadResponse => true,
        FtpError::UnexpectedResponse(response) => {
            // 421: the server is closing the control connection.
            response.status.code() == 421
        }
        _ => false,
    }
}

/// Whether a command that `FEAT` advertised was refused in a way that means
/// "ask me something simpler" rather than "that failed".
fn retry_without(err: &Error) -> bool {
    match err {
        Error::Unsupported(_) => true,
        Error::Io { .. }
        | Error::Bare(_)
        | Error::Config { .. }
        | Error::Binding { .. }
        | Error::Msg(_)
        | Error::NotFound(_)
        | Error::InvalidPath(_)
        | Error::Cancelled
        | Error::ConnectionLost(_)
        | Error::ConnectionClosed(_) => false,
    }
}

/// This crate's error for one of suppaftp's.
///
/// It names the connection and the command, and it never carries a credential:
/// the only string that reaches here from the wire is the server's own reply,
/// and a server does not echo `PASS`.
fn translate(err: FtpError, authority: &str, what: &'static str) -> Error {
    match err {
        FtpError::ConnectionError(_) => Error::ConnectionLost(authority.to_string()),
        FtpError::UnexpectedResponse(response) => {
            let code = response.status.code();
            let body = response.as_string().unwrap_or_default();
            match code {
                421 => Error::ConnectionLost(authority.to_string()),
                550 | 551 | 553 => Error::NotFound(format!("{authority}: {body}")),
                // The server does not have this command, or will not take it
                // with these arguments: the caller asks something simpler.
                500 | 502 | 504 => Error::Unsupported(what),
                _ => Error::msg(format!("{authority}: {what}: {body}")),
            }
        }
        FtpError::BadResponse => Error::msg(format!(
            "{authority}: {what}: the server's reply could not be understood"
        )),
        FtpError::InvalidAddress(err) => Error::msg(format!("{authority}: {what}: {err}")),
        FtpError::SecureError(message) => {
            Error::msg(format!("{authority}: {what}: TLS failed: {message}"))
        }
        FtpError::DataConnectionAlreadyOpen => Error::msg(format!(
            "{authority}: {what}: a transfer is already running on this connection"
        )),
    }
}

/// The same, for the two places that have to answer with [`std::io::Error`]
/// because they are inside a [`Read`] or a [`Write`].
fn as_io(err: FtpError, authority: &str) -> std::io::Error {
    let fatal = fatal(&err);
    let message = translate(err, authority, "the transfer").to_string();
    if fatal {
        std::io::Error::new(std::io::ErrorKind::ConnectionAborted, message)
    } else {
        std::io::Error::other(message)
    }
}

#[cfg(test)]
mod tests {
    use super::parse::{parse_list, parse_mlsd, utc};
    use super::pool::dead;
    use super::tls::{base64_decode, pem_certificates};
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex as StdMutex;
    use suppaftp::Status;
    use suppaftp::types::Response;

    // ---------------------------------------------------------- fixtures ----

    /// Captured `MLSD` lines. vsftpd and ProFTPD write the facts in different
    /// orders and different cases, which is why the parser is case-insensitive
    /// and order-independent.
    const MLSD_LINES: &[&str] = &[
        "type=dir;sizd=4096;modify=20240110134600;UNIX.mode=0755;UNIX.owner=ftp;UNIX.group=ftp; pub",
        "type=file;size=1048576;modify=20240110134601;UNIX.mode=0644;UNIX.uid=1000;UNIX.gid=1000; disk image.iso",
        "modify=20231231235959;perm=fle;type=file;size=12; readme.txt",
        "Type=cdir;Modify=20240110134600; /srv/media",
        "type=pdir;modify=20240110134600; ..",
        "type=OS.unix=slink:/srv/media/1.2.3;modify=20240110134602; latest",
    ];

    /// Captured Unix `LIST` lines: vsftpd, ProFTPD, pure-ftpd and a plain
    /// `ls -l` behind a server that shells out.
    const UNIX_LINES: &[&str] = &[
        "drwxr-xr-x    2 ftp      ftp          4096 Jan 10 13:46 pub",
        "-rw-r--r--    1 1000     1000      1048576 Jan 10  2024 disk image.iso",
        "-rw-r--r--    1 ftp      ftp            12 Dec 31  2023 readme.txt",
        "lrwxrwxrwx    1 root     root            5 Feb 14 09:03 latest -> 1.2.3",
        "drwxrwsr-x+   4 nobody   nogroup      4096 Mar  3 07:00 shared",
        "-rw-r--r--    1 ftp      ftp          1024 2024-01-10 13:46 iso-dated.txt",
    ];

    /// Captured IIS `LIST` lines in MS-DOS listing mode.
    const DOS_LINES: &[&str] = &[
        "01-10-24  01:46PM       <DIR>          program files",
        "01-10-24  01:46PM              1048576 disk image.iso",
        "03-03-2024  07:00AM                 12 readme.txt",
    ];

    /// Dialects this backend does not read: VMS, NetWare and EPLF.
    const FOREIGN_LINES: &[&str] = &[
        "FILE.TXT;1              1  10-JAN-2024 13:46 [GROUP,OWNER] (RWED,RWED,RE,)",
        "d [R----F--] supervisor            512       Jan 10 13:46    pub",
        "+i8388621.29609,m1704893160,r,s1048576,\tdisk.iso",
    ];

    // ------------------------------------------------------- MLSD parsing ----

    #[test]
    fn mlsd_reads_the_type_the_size_and_the_time() {
        let Some(dir) = parse_mlsd(MLSD_LINES[0]) else {
            panic!("the directory line did not parse");
        };
        assert_eq!(dir.name, "pub");
        assert_eq!(dir.kind, EntryKind::Dir);
        assert_eq!(dir.size, 0);
        assert_eq!(dir.mtime, utc(2024, 1, 10, 13, 46, 0));

        let Some(file) = parse_mlsd(MLSD_LINES[1]) else {
            panic!("the file line did not parse");
        };
        assert_eq!(file.name, "disk image.iso");
        assert_eq!(file.kind, EntryKind::File);
        assert_eq!(file.size, 1_048_576);
        assert_eq!(file.mtime, utc(2024, 1, 10, 13, 46, 1));
    }

    #[test]
    fn mlsd_facts_are_order_and_case_independent() {
        let Some(entry) = parse_mlsd(MLSD_LINES[2]) else {
            panic!("the reordered line did not parse");
        };
        assert_eq!(entry.name, "readme.txt");
        assert_eq!(entry.size, 12);
        assert_eq!(entry.mtime, utc(2023, 12, 31, 23, 59, 59));
    }

    #[test]
    fn mlsd_drops_the_directory_and_its_parent() {
        // `RemoteFs::read_dir` adds the `..` row; a listing that carried
        // its own would show two.
        assert!(parse_mlsd(MLSD_LINES[3]).is_none());
        assert!(parse_mlsd(MLSD_LINES[4]).is_none());
    }

    #[test]
    fn mlsd_never_reports_a_symlink() {
        let Some(entry) = parse_mlsd(MLSD_LINES[5]) else {
            panic!("the link line did not parse");
        };
        assert_eq!(entry.name, "latest");
        assert_eq!(entry.kind, EntryKind::Other);
    }

    #[test]
    fn mlsd_refuses_a_line_that_carries_no_facts() {
        assert!(parse_mlsd("just a name").is_none());
        assert!(parse_mlsd("").is_none());
        assert!(parse_mlsd("type=file;size=1;").is_none());
    }

    // ------------------------------------------------------- LIST parsing ----

    #[test]
    fn unix_list_reads_a_directory_and_a_file() {
        let Some(dir) = parse_list(UNIX_LINES[0]) else {
            panic!("the directory line did not parse");
        };
        assert_eq!(dir.name, "pub");
        assert_eq!(dir.kind, EntryKind::Dir);
        assert_eq!(dir.size, 0);

        let Some(file) = parse_list(UNIX_LINES[1]) else {
            panic!("the file line did not parse");
        };
        // The name keeps its spaces: it is taken from the original line by
        // offset rather than from a token.
        assert_eq!(file.name, "disk image.iso");
        assert_eq!(file.size, 1_048_576);
        assert_eq!(file.mtime, utc(2024, 1, 10, 0, 0, 0));
    }

    #[test]
    fn unix_list_reads_the_acl_marker_and_the_iso_date() {
        let Some(shared) = parse_list(UNIX_LINES[4]) else {
            panic!("the ACL line did not parse");
        };
        assert_eq!(shared.name, "shared");
        assert_eq!(shared.kind, EntryKind::Dir);

        let Some(dated) = parse_list(UNIX_LINES[5]) else {
            panic!("the ISO-dated line did not parse");
        };
        assert_eq!(dated.name, "iso-dated.txt");
        assert_eq!(dated.size, 1024);
        assert_eq!(dated.mtime, utc(2024, 1, 10, 13, 46, 0));
    }

    #[test]
    fn unix_list_strips_a_link_target_and_reports_no_link() {
        let Some(entry) = parse_list(UNIX_LINES[3]) else {
            panic!("the link line did not parse");
        };
        assert_eq!(entry.name, "latest");
        assert_eq!(entry.kind, EntryKind::Other);
    }

    #[test]
    fn unix_list_drops_the_dot_rows() {
        assert!(parse_list("drwxr-xr-x  2 ftp ftp 4096 Jan 10 13:46 .").is_none());
        assert!(parse_list("drwxr-xr-x  2 ftp ftp 4096 Jan 10 13:46 ..").is_none());
    }

    /// the three listing dialects agree that a name is a name.
    ///
    /// `MLSD` always reduced a row to its last component; `LIST` and the DOS
    /// dialect did not, so the same server was safe over one and not over the
    /// other - and which one is used is the server's choice, since `LIST` is
    /// the fallback whenever `FEAT` omits `MLSD`. A name carrying `../` is
    /// joined onto a local destination by `ops::copy` as written, which is Zip
    /// Slip's remote spelling.
    #[test]
    fn every_listing_dialect_reduces_a_name_to_a_name() {
        let unix = parse_list(
            "-rw-r--r--   1 ftp      ftp            10 Jan 10  2024 ../../../../etc/cron.d/pwn",
        );
        assert_eq!(unix.map(|e| e.name), Some("pwn".to_string()));

        let dos = parse_list("01-10-24  01:46PM              1234 ../../../../etc/cron.d/dos");
        assert_eq!(dos.map(|e| e.name), Some("dos".to_string()));

        let mlsd = parse_mlsd("type=file;size=10; ../../../../etc/cron.d/pwn");
        assert_eq!(mlsd.map(|e| e.name), Some("pwn".to_string()));

        // A row that basenames to nothing usable is no row at all.
        assert!(parse_list("-rw-r--r--   1 ftp ftp 10 Jan 10  2024 ../..").is_none());
        assert!(parse_mlsd("type=file;size=10; /").is_none());
        // An ordinary name with an awkward look is untouched.
        let ordinary = parse_list("-rw-r--r--   1 ftp ftp 10 Jan 10  2024 ...odd name");
        assert_eq!(ordinary.map(|e| e.name), Some("...odd name".to_string()));
    }

    #[test]
    fn dos_list_reads_a_directory_a_size_and_a_twelve_hour_clock() {
        let Some(dir) = parse_list(DOS_LINES[0]) else {
            panic!("the DIR line did not parse");
        };
        assert_eq!(dir.name, "program files");
        assert_eq!(dir.kind, EntryKind::Dir);

        let Some(file) = parse_list(DOS_LINES[1]) else {
            panic!("the DOS file line did not parse");
        };
        assert_eq!(file.name, "disk image.iso");
        assert_eq!(file.size, 1_048_576);
        assert_eq!(file.mtime, utc(2024, 1, 10, 13, 46, 0));

        let Some(morning) = parse_list(DOS_LINES[2]) else {
            panic!("the four-digit-year line did not parse");
        };
        assert_eq!(morning.name, "readme.txt");
        assert_eq!(morning.mtime, utc(2024, 3, 3, 7, 0, 0));
    }

    #[test]
    fn a_dialect_this_backend_does_not_know_is_none_and_not_a_guess() {
        for line in FOREIGN_LINES {
            assert!(
                parse_list(line).is_none(),
                "{line:?} was parsed by a dialect it does not belong to"
            );
        }
    }

    #[test]
    fn a_listing_in_an_unknown_dialect_is_an_error_and_not_an_empty_directory() {
        let lines: Vec<String> = FOREIGN_LINES.iter().map(|l| (*l).to_string()).collect();
        let Err(err) = entries_from_list("/pub", &lines) else {
            panic!("an unreadable listing was reported as a directory");
        };
        let message = err.to_string();
        assert!(message.contains("/pub"), "{message}");
        assert!(message.contains("LIST"), "{message}");
    }

    #[test]
    fn a_listing_keeps_the_rows_it_understood_and_skips_the_ones_it_did_not() {
        let mut lines: Vec<String> = vec!["total 12".to_string()];
        lines.extend(UNIX_LINES.iter().map(|l| (*l).to_string()));
        lines.push(FOREIGN_LINES[0].to_string());
        let Ok(entries) = entries_from_list("/pub", &lines) else {
            panic!("a readable listing was refused");
        };
        assert_eq!(entries.len(), UNIX_LINES.len());
    }

    #[test]
    fn an_empty_directory_is_an_empty_listing_and_not_an_error() {
        let lines = vec!["total 0".to_string()];
        let Ok(entries) = entries_from_list("/pub", &lines) else {
            panic!("an empty directory was refused");
        };
        assert!(entries.is_empty());
    }

    /// I13: no `Entry` from this backend carries a mode, an owner or a link.
    #[test]
    fn no_row_from_this_backend_has_a_mode_an_owner_or_a_link() {
        let mut rows: Vec<Entry> = MLSD_LINES.iter().filter_map(|l| parse_mlsd(l)).collect();
        rows.extend(UNIX_LINES.iter().filter_map(|l| parse_list(l)));
        rows.extend(DOS_LINES.iter().filter_map(|l| parse_list(l)));
        assert!(rows.len() >= UNIX_LINES.len());
        for row in rows {
            assert_eq!(row.mode, 0, "{} carried a mode", row.name);
            assert_eq!(row.uid, 0, "{} carried a uid", row.name);
            assert_eq!(row.gid, 0, "{} carried a gid", row.name);
            assert!(
                !matches!(row.kind, EntryKind::Symlink { .. }),
                "{} was reported as a symlink",
                row.name
            );
        }
    }

    // -------------------------------------------------------------- paths ----

    #[test]
    fn paths_are_absolute_and_lose_their_trailing_slash() {
        assert_eq!(normalize("/srv/media/"), "/srv/media");
        assert_eq!(normalize("srv/media"), "/srv/media");
        assert_eq!(normalize(""), "/");
        assert_eq!(normalize("/"), "/");
        assert_eq!(normalize("///"), "/");
    }

    #[test]
    fn a_path_with_a_line_break_is_refused_before_it_reaches_the_server() {
        // An FTP command is one line, so this would be a second command.
        let Err(err) = check_path("/srv/media\r\nDELE /etc/passwd") else {
            panic!("a path with a line break was accepted");
        };
        assert!(err.to_string().contains("line break"), "{err}");
        assert!(check_path("/srv/media/a file.iso").is_ok());
    }

    #[test]
    fn splitting_a_path_stops_at_the_root() {
        assert_eq!(
            split_path("/srv/media/a.iso"),
            Some(("/srv/media".to_string(), "a.iso".to_string()))
        );
        assert_eq!(
            split_path("/a.iso"),
            Some(("/".to_string(), "a.iso".to_string()))
        );
        assert_eq!(split_path("/"), None);
    }

    #[test]
    fn an_anonymous_login_is_never_prompted() {
        // The rule itself is `auth::is_anonymous`, and it is tested there;
        // what matters here is that this backend asks it rather than keeping
        // a second copy of it.
        assert!(is_anonymous(ANONYMOUS_USER));
        assert!(is_anonymous(""));
        assert!(!is_anonymous("thorin"));

        let mut server = FakeServer::default();
        server.list.insert("/".to_string(), Vec::new());
        let log = Arc::clone(&server.log);
        let mut session: Box<dyn Session> = Box::new(server);
        let store = crate::remote::keyring::NoKeyring;
        // The plan says "ask for a password"; the user name says nobody has
        // to be asked. The user name wins, which is the whole rule.
        let credential = authenticate(
            session.as_mut(),
            &target(),
            AuthPlan::for_password_login(None),
            None,
            &store,
            // Hooks that would panic if they were ever asked: an anonymous
            // login must not put a prompt on the screen.
            &BlockingHooks::new(|_, _| panic!("an anonymous login asked for a password")),
        );
        assert!(credential.is_ok());
        assert_eq!(logged(&log), vec!["LOGIN anonymous".to_string()]);
    }

    // ---------------------------------------------------------------- TLS ----

    #[test]
    fn pem_blocks_decode_and_anything_else_is_skipped() {
        let pem = "\
# a comment
-----BEGIN CERTIFICATE-----
aGVsbG8gd29ybGQ=
-----END CERTIFICATE-----
-----BEGIN TRUSTED CERTIFICATE-----
bm90IHRoaXMgb25l
-----END TRUSTED CERTIFICATE-----
-----BEGIN CERTIFICATE-----
c2Vjb25k
-----END CERTIFICATE-----
";
        let certs = pem_certificates(pem);
        assert_eq!(certs.len(), 2);
        assert_eq!(certs.first().map(Vec::as_slice), Some(&b"hello world"[..]));
        assert_eq!(certs.get(1).map(Vec::as_slice), Some(&b"second"[..]));
    }

    #[test]
    fn base64_decodes_padding_and_refuses_rubbish() {
        assert_eq!(base64_decode("aGVsbG8="), Some(b"hello".to_vec()));
        assert_eq!(base64_decode("aGVsbG8"), Some(b"hello".to_vec()));
        assert_eq!(base64_decode(""), Some(Vec::new()));
        assert_eq!(base64_decode("not base64!"), None);
    }

    // -------------------------------------------------------------- errors ----

    fn response(code: u32, body: &str) -> FtpError {
        FtpError::UnexpectedResponse(Response::new(Status::from(code), body.as_bytes().to_vec()))
    }

    #[test]
    fn a_missing_file_is_not_found_and_a_closed_control_channel_is_lost() {
        let missing = translate(response(550, "550 No such file"), "ftp://a@h:21", "RETR");
        assert!(matches!(missing, Error::NotFound(_)), "{missing:?}");

        let closing = translate(response(421, "421 Bye"), "ftp://a@h:21", "LIST");
        assert!(matches!(closing, Error::ConnectionLost(_)), "{closing:?}");

        let broken = translate(
            FtpError::ConnectionError(std::io::Error::from(std::io::ErrorKind::BrokenPipe)),
            "ftp://a@h:21",
            "LIST",
        );
        assert!(matches!(broken, Error::ConnectionLost(_)), "{broken:?}");

        let unknown = translate(response(500, "500 Unknown command"), "ftp://a@h:21", "MLSD");
        assert!(matches!(unknown, Error::Unsupported("MLSD")), "{unknown:?}");
        assert!(retry_without(&unknown));
    }

    #[test]
    fn a_refused_command_does_not_cost_the_pool_a_connection() {
        assert!(!fatal(&response(550, "550 No such file")));
        assert!(fatal(&response(421, "421 Bye")));
        assert!(fatal(&FtpError::ConnectionError(std::io::Error::from(
            std::io::ErrorKind::BrokenPipe
        ))));
    }

    // ---------------------------------------------------- a fake server ----

    /// A [`Session`] with no socket under it, so that everything above the
    /// wire is testable with no server (the design says
    /// plainly that there is no FTP server on this machine to test against).
    #[derive(Default)]
    struct FakeServer {
        /// One directory's raw `MLSD` lines, by path.
        mlsd: HashMap<String, Vec<String>>,
        /// One directory's raw `LIST` lines, by path.
        list: HashMap<String, Vec<String>>,
        /// The bytes `RETR` hands back.
        content: Vec<u8>,
        /// Every command that was issued, in order.
        log: Arc<StdMutex<Vec<String>>>,
        /// What `MLSD` answers with, when it is not a listing.
        mlsd_error: Option<u32>,
        /// How long `QUIT` blocks before answering, for the "a farewell is
        /// never said on the event loop" test. A server that has been `kill
        /// -STOP`ped never answers at all, which this stands in for.
        quit_delay: Duration,
    }

    impl FakeServer {
        fn note(&self, what: &str) {
            self.log
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(what.to_string());
        }
    }

    impl Session for FakeServer {
        fn login(&mut self, user: &str, _password: &str) -> FtpResult<()> {
            self.note(&format!("LOGIN {user}"));
            Ok(())
        }
        fn binary(&mut self) -> FtpResult<()> {
            self.note("TYPE I");
            Ok(())
        }
        fn set_read_timeout(&mut self, _timeout: Option<Duration>) {}
        fn pwd(&mut self) -> FtpResult<String> {
            Ok("/srv/media".to_string())
        }
        fn feat(&mut self) -> FtpResult<Features> {
            Ok(Features::new())
        }
        fn mlsd(&mut self, dir: &str) -> FtpResult<Vec<String>> {
            self.note(&format!("MLSD {dir}"));
            if let Some(code) = self.mlsd_error {
                return Err(response(code, "no"));
            }
            Ok(self.mlsd.get(dir).cloned().unwrap_or_default())
        }
        fn list(&mut self, dir: &str) -> FtpResult<Vec<String>> {
            self.note(&format!("LIST {dir}"));
            Ok(self.list.get(dir).cloned().unwrap_or_default())
        }
        fn mlst(&mut self, path: &str) -> FtpResult<String> {
            self.note(&format!("MLST {path}"));
            Err(response(500, "no"))
        }
        fn size(&mut self, path: &str) -> FtpResult<u64> {
            self.note(&format!("SIZE {path}"));
            Err(response(550, "no"))
        }
        fn mdtm(&mut self, path: &str) -> FtpResult<Option<SystemTime>> {
            self.note(&format!("MDTM {path}"));
            Ok(None)
        }
        fn mkdir(&mut self, path: &str) -> FtpResult<()> {
            self.note(&format!("MKD {path}"));
            Ok(())
        }
        fn rm(&mut self, path: &str) -> FtpResult<()> {
            self.note(&format!("DELE {path}"));
            Ok(())
        }
        fn rmdir(&mut self, path: &str) -> FtpResult<()> {
            self.note(&format!("RMD {path}"));
            Ok(())
        }
        fn rename(&mut self, from: &str, to: &str) -> FtpResult<()> {
            self.note(&format!("RNFR {from} RNTO {to}"));
            Ok(())
        }
        fn retr_start(&mut self, path: &str) -> FtpResult<Box<dyn Read + Send>> {
            self.note(&format!("RETR {path}"));
            Ok(Box::new(std::io::Cursor::new(self.content.clone())))
        }
        fn retr_finish(&mut self, _data: Box<dyn Read + Send>) -> FtpResult<()> {
            self.note("RETR done");
            Ok(())
        }
        fn retr_abort(&mut self, _data: Box<dyn Read + Send>) -> FtpResult<()> {
            self.note("ABOR");
            Ok(())
        }
        fn stor_start(&mut self, path: &str) -> FtpResult<Box<dyn Write + Send>> {
            self.note(&format!("STOR {path}"));
            Ok(Box::new(std::io::sink()))
        }
        fn stor_finish(&mut self, _data: Box<dyn Write + Send>) -> FtpResult<()> {
            self.note("STOR done");
            Ok(())
        }
        fn quit(&mut self) -> FtpResult<()> {
            std::thread::sleep(self.quit_delay);
            self.note("QUIT");
            Ok(())
        }
    }

    fn target() -> Target {
        Target {
            protocol: Protocol::Ftp,
            host: "ftp.example.org".to_string(),
            port: 21,
            user: ANONYMOUS_USER.to_string(),
            dir: None,
        }
    }

    /// An [`FtpFs`] over a fake server, with `FEAT`'s answers set by hand.
    fn fake_fs(server: FakeServer, mlsd: bool) -> (FtpFs, Arc<StdMutex<Vec<String>>>) {
        let log = Arc::clone(&server.log);
        let target = target();
        let pool = Pool::new(target.authority(), Box::new(server));
        let fs = FtpFs {
            target,
            start: "/srv/media".to_string(),
            pool,
            facts: ServerFacts {
                mlsd: AtomicBool::new(mlsd),
                mlst: AtomicBool::new(false),
                size: AtomicBool::new(false),
                mdtm: AtomicBool::new(false),
            },
        };
        (fs, log)
    }

    fn logged(log: &Arc<StdMutex<Vec<String>>>) -> Vec<String> {
        log.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Wait for a command to reach the fake server, for the one command that
    /// is sent off the caller's thread.
    ///
    /// `Pool::close` hands its farewells to [`farewell`]'s thread on purpose
    /// (it is reached from `dispatch`, which may not block), so the assertion
    /// that `QUIT` was sent is a wait rather than a read. It fails by timing
    /// out and never by hanging.
    fn wait_for(log: &Arc<StdMutex<Vec<String>>>, command: &str) -> bool {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            if logged(log).iter().any(|line| line == command) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }

    #[test]
    fn a_listing_prefers_mlsd_and_returns_rows_without_a_parent_row() {
        let mut server = FakeServer::default();
        server.mlsd.insert(
            "/srv/media".to_string(),
            MLSD_LINES.iter().map(|l| (*l).to_string()).collect(),
        );
        let (fs, log) = fake_fs(server, true);
        let Ok(entries) = fs.list("/srv/media/") else {
            panic!("the listing failed");
        };
        // Six captured lines, two of which are the directory and its parent.
        assert_eq!(entries.len(), 4);
        assert!(entries.iter().all(|e| !e.is_parent));
        assert_eq!(logged(&log), vec!["MLSD /srv/media".to_string()]);
    }

    #[test]
    fn a_server_that_advertises_mlsd_and_then_refuses_it_falls_back_to_list() {
        let mut server = FakeServer {
            mlsd_error: Some(500),
            ..FakeServer::default()
        };
        server.list.insert(
            "/srv/media".to_string(),
            UNIX_LINES.iter().map(|l| (*l).to_string()).collect(),
        );
        let (fs, log) = fake_fs(server, true);
        let Ok(entries) = fs.list("/srv/media") else {
            panic!("the fallback listing failed");
        };
        assert_eq!(entries.len(), UNIX_LINES.len());
        assert_eq!(
            logged(&log),
            vec!["MLSD /srv/media".to_string(), "LIST /srv/media".to_string()]
        );
        // And it does not ask again on the next listing.
        let _ = fs.list("/srv/media");
        assert_eq!(logged(&log).len(), 3);
    }

    #[test]
    fn a_server_that_answers_mlsd_with_something_else_falls_back_to_list() {
        // vsftpd has been seen to advertise MLSD and answer with LIST output.
        // An empty panel would be a claim that the directory is empty.
        let mut server = FakeServer::default();
        server.mlsd.insert(
            "/srv/media".to_string(),
            UNIX_LINES.iter().map(|l| (*l).to_string()).collect(),
        );
        server.list.insert(
            "/srv/media".to_string(),
            UNIX_LINES.iter().map(|l| (*l).to_string()).collect(),
        );
        let (fs, log) = fake_fs(server, true);
        let Ok(entries) = fs.list("/srv/media") else {
            panic!("the fallback listing failed");
        };
        assert_eq!(entries.len(), UNIX_LINES.len());
        assert_eq!(
            logged(&log),
            vec!["MLSD /srv/media".to_string(), "LIST /srv/media".to_string()]
        );
    }

    #[test]
    fn stat_falls_back_to_the_parent_listing_when_the_server_has_no_mlst() {
        let mut server = FakeServer::default();
        server.list.insert(
            "/srv/media".to_string(),
            UNIX_LINES.iter().map(|l| (*l).to_string()).collect(),
        );
        let (fs, _log) = fake_fs(server, false);
        let Ok(entry) = fs.stat("/srv/media/disk image.iso") else {
            panic!("stat failed");
        };
        assert_eq!(entry.name, "disk image.iso");
        assert_eq!(entry.size, 1_048_576);

        let missing = fs.stat("/srv/media/nothing.txt");
        assert!(matches!(missing, Err(Error::NotFound(_))), "{missing:?}");
    }

    #[test]
    fn reading_to_the_end_finishes_the_transfer_and_gives_the_connection_back() {
        let server = FakeServer {
            content: b"hello".to_vec(),
            ..FakeServer::default()
        };
        let (fs, log) = fake_fs(server, false);
        let Ok(mut reader) = fs.open_read("/srv/media/a.txt") else {
            panic!("RETR failed");
        };
        // While the transfer is live the only connection is out of the pool.
        assert_eq!(fs.pool.lock().idle.len(), 0);
        let mut buf = Vec::new();
        let Ok(n) = reader.read_to_end(&mut buf) else {
            panic!("the read failed");
        };
        assert_eq!(n, 5);
        drop(reader);
        assert_eq!(
            logged(&log),
            vec!["RETR /srv/media/a.txt".to_string(), "RETR done".to_string()]
        );
        assert_eq!(fs.pool.lock().idle.len(), 1);
    }

    #[test]
    fn a_reader_dropped_early_aborts_rather_than_draining_the_file() {
        let server = FakeServer {
            content: vec![0u8; 4096],
            ..FakeServer::default()
        };
        let (fs, log) = fake_fs(server, false);
        let Ok(mut reader) = fs.open_read("/srv/media/big.bin") else {
            panic!("RETR failed");
        };
        let mut buf = [0u8; 16];
        let Ok(_) = reader.read(&mut buf) else {
            panic!("the read failed");
        };
        drop(reader);
        assert!(logged(&log).contains(&"ABOR".to_string()));
        // ABOR leaves the command channel at the start of the next reply, so
        // the connection goes back into the pool rather than being thrown
        // away: cancelling one `F3` must not cost the panel its connection.
        assert!(fs.is_live());
        assert_eq!(fs.pool.lock().idle.len(), 1);
    }

    #[test]
    fn flush_is_the_commit_and_a_second_flush_is_not_a_second_commit() {
        let (fs, log) = fake_fs(FakeServer::default(), false);
        let Ok(mut writer) = fs.open_write("/srv/media/new.txt") else {
            panic!("STOR failed");
        };
        let Ok(()) = writer.write_all(b"hello") else {
            panic!("the write failed");
        };
        let Ok(()) = writer.flush() else {
            panic!("the commit failed");
        };
        let Ok(()) = writer.flush() else {
            panic!("the second flush failed");
        };
        drop(writer);
        assert_eq!(
            logged(&log),
            vec![
                "STOR /srv/media/new.txt".to_string(),
                "STOR done".to_string()
            ]
        );
        assert_eq!(fs.pool.lock().idle.len(), 1);
    }

    #[test]
    fn a_writer_dropped_without_a_flush_still_closes_the_transfer() {
        let (fs, log) = fake_fs(FakeServer::default(), false);
        let Ok(mut writer) = fs.open_write("/srv/media/new.txt") else {
            panic!("STOR failed");
        };
        let Ok(()) = writer.write_all(b"hello") else {
            panic!("the write failed");
        };
        drop(writer);
        assert!(logged(&log).contains(&"STOR done".to_string()));
        assert_eq!(fs.pool.lock().idle.len(), 1);
    }

    #[test]
    fn what_ftp_cannot_do_is_refused_rather_than_faked() {
        let (fs, _log) = fake_fs(FakeServer::default(), false);
        assert!(matches!(
            fs.read_link("/srv/media/latest"),
            Err(Error::Unsupported(_))
        ));
        assert!(matches!(
            fs.open_seek("/srv/media/a.iso"),
            Err(Error::Unsupported(_))
        ));
        // The refusal and the capability have to agree.
        assert!(!fs.capabilities().seekable);
        assert_eq!(fs.capabilities(), Capabilities::FTP);
        assert_eq!(fs.protocol(), Protocol::Ftp);
    }

    #[test]
    fn closing_the_pool_ends_every_later_command() {
        let (fs, log) = fake_fs(FakeServer::default(), false);
        assert!(fs.is_live());
        fs.close();
        assert!(!fs.is_live(), "close answers at once, before any QUIT");
        assert!(wait_for(&log, "QUIT"), "the farewell is still said");
        let after = fs.create_dir("/srv/media/new");
        assert!(matches!(after, Err(Error::ConnectionLost(_))), "{after:?}");
        // Idempotent, which is what `RemoteRegistry::close` needs of it.
        fs.close();
    }

    /// `dispatch` does no I/O, and closing an FTP
    /// connection is reached straight from it - `Ctrl+F` and Y, a tab closed,
    /// a panel navigated off a connection.
    ///
    /// `QUIT` is a round trip per pooled connection, on sockets whose read
    /// timeout was deliberately dropped after login, so saying the farewells
    /// inline froze the TUI against a server that had stopped answering: no
    /// redraw, no keys, not even the signal arms of the event loop. Here the
    /// server takes an hour to answer `QUIT` and `close` still returns at
    /// once.
    #[test]
    fn closing_a_connection_never_waits_for_the_farewell() {
        let server = FakeServer {
            quit_delay: Duration::from_secs(3600),
            ..FakeServer::default()
        };
        let (fs, log) = fake_fs(server, false);
        let started = std::time::Instant::now();
        fs.close();
        let took = started.elapsed();
        assert!(
            took < Duration::from_secs(1),
            "close blocked on the farewell for {took:?}"
        );
        assert!(!fs.is_live(), "and the pool is dead to every later command");
        assert!(
            !logged(&log).contains(&"QUIT".to_string()),
            "the QUIT is still out there on its own thread, which is the point"
        );
        // Nothing is joined and nothing is waited for: the farewell thread
        // outlives this test and dies with the process, which costs the server
        // an idle timeout and nothing else.
    }

    #[test]
    fn a_command_that_kills_the_connection_takes_it_out_of_the_pool() {
        struct Dying;
        impl Session for Dying {
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

        let target = target();
        let pool = Pool::new(target.authority(), Box::new(Dying));
        let fs = FtpFs {
            target,
            start: "/".to_string(),
            pool,
            facts: ServerFacts {
                mlsd: AtomicBool::new(false),
                mlst: AtomicBool::new(false),
                size: AtomicBool::new(false),
                mdtm: AtomicBool::new(false),
            },
        };
        let failed = fs.remove_file("/srv/media/a.txt");
        assert!(
            matches!(failed, Err(Error::ConnectionLost(_))),
            "{failed:?}"
        );
        // The pool has nothing left, so the panel is told the connection is
        // gone rather than being made to wait for a connection that will never
        // come back.
        assert!(!fs.is_live());
        let again = fs.remove_file("/srv/media/a.txt");
        assert!(matches!(again, Err(Error::ConnectionLost(_))), "{again:?}");
    }

    #[test]
    fn debug_says_where_it_points_and_nothing_else() {
        let (fs, _log) = fake_fs(FakeServer::default(), false);
        let text = format!("{fs:?}");
        assert!(text.contains("ftp.example.org"), "{text}");
        assert!(!text.to_lowercase().contains("password"), "{text}");
        assert_eq!(format!("{:?}", BlockingHooks::none()), "BlockingHooks");
    }
}
