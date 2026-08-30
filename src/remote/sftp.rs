//! SFTP: the "default and primary target".
//!
//! `russh` speaks SSH and `russh-sftp` speaks the SFTP subsystem, version 3.
//! Everything else in this module exists to put a **synchronous** transport in
//! front of them, because that is what the rest of the program is.
//!
//!
//! # The shape
//!
//! ```text
//! blocking pool                  one tokio task                the wire
//! -------------                  --------------                --------
//! SftpFs::list(dir)
//!   Command::List  ------------> actor::run
//!   park on recv                   tokio::spawn -------------> OPENDIR
//!                                                              READDIR ...
//!   <----------------------------- reply.send(rows)            CLOSE
//! ```
//!
//! * The command channel is `tokio::sync::mpsc::unbounded_channel`. Its
//!   `send` is not `async`, never blocks and never panics; it fails only when
//!   the actor is gone, which is exactly the "the connection is closed"
//!   answer. Unbounded is safe because the backpressure is elsewhere: a
//!   calling thread has at most one command in flight.
//! * Every reply channel is `std::sync::mpsc::sync_channel(1)`. Its `recv`
//!   blocks the calling thread and cannot panic on any thread, which
//!   `blocking_recv` and `block_on` both can. The house style forbids panic
//!   paths, and a rule enforced by the choice of primitive cannot be broken by
//!   a call site added later.
//!
//! # Security
//!
//! * The host key is verified against `~/.ssh/known_hosts` **before any
//!   authentication method is attempted** (invariant S7). There
//!   is no configuration value, environment variable or argument in this
//!   module that skips it, and there is no path from a changed or a revoked
//!   key to a connection: [`ClientHandler::check_server_key`] returns `false`
//!   for both and never reaches [`crate::remote::known_hosts::learn`]
//!   (invariant S6).
//! * A [`Secret`] appears in exactly two places here: the local variable that
//!   holds the answer to a prompt, and the argument to russh's authenticate
//!   call. It is never in [`SftpFs`], never in a
//!   `Debug`, never in an error and never in a log line.

pub(crate) mod actor;
pub(crate) mod attrs;
pub(crate) mod io;

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use russh::client::{AuthResult, Handle};
use russh::keys::{Algorithm, PrivateKeyWithHashAlg, PublicKey, PublicKeyOrCertificate};
use tokio::net::TcpStream;
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

use crate::app::RemoteEvent;
use crate::config::RemoteConfig;
use crate::dialog::SecretAnswer;
use crate::error::{Error, Result};
use crate::remote::connect::ConnectId;
use crate::vfs::{Capabilities, Entry, ReadSeek};

use super::auth::{AuthPlan, MAX_ASKS, Method, Outcome, SecretKind};
use super::keyring::{self, SecretStore};
use super::known_hosts::{self, Verdict};
use super::secret::Secret;
use super::transport::RemoteTransport;
use super::{Protocol, Target};

use actor::{Command, Context, Pool, Reply, WireLimits};
use attrs::SAFE_PAYLOAD;
use io::PipelinedReader;

/// How many chunks are in flight in each direction, when `remote.pipeline`
/// says nothing.
pub const PIPELINE_DEPTH: usize = 4;

/// How many missed keepalives end a connection.
///
///
/// With `remote.keepalive` at its default of 30 s, a dead link is noticed in
/// about 90 s rather than never. russh counts them itself; this is the number
/// handed to it.
pub const KEEPALIVE_MISSES: usize = 3;

/// The largest window one read or write request will ever ask for.
///
/// A server that announces `limits@openssh.com` can raise the wire payload a
/// long way, and a read-ahead of `remote.pipeline` times an unbounded window
/// would be a memory footprint nobody asked for. One megabyte times the
/// default depth of four is four megabytes in flight, which is already more
/// than a gigabit link needs.
pub const MAX_WINDOW: usize = 1024 * 1024;

/// How a connect attempt asks the user a question.
///
/// The connect task is async and long-lived and the dialogs are on the event
/// loop, so a question goes out as a [`RemoteEvent`] carrying a
/// `oneshot::Sender` and the answer comes back through it.
/// **Dropping that sender is a refusal**, which is how `Esc` cancels a connect
/// with no extra code path.
///
/// the design names this type in [`SftpFs::connect`]'s signature
/// without saying where it lives. It is here because this is the only module
/// that constructs one; see the report's list of deviations.
#[derive(Clone)]
pub struct ConnectHooks {
    /// Where a question goes.
    events: tokio::sync::mpsc::Sender<RemoteEvent>,
    /// Which attempt is asking, so an answer to an abandoned attempt is
    /// dropped rather than applied.
    ///
    /// Held as the `u64` inside [`ConnectId`] rather than as the id, because
    /// [`ConnectId`] is another module's type and its derives are not this
    /// module's to depend on; the field is public, so rebuilding it is always
    /// possible and never a clone.
    attempt: u64,
    /// The `known_hosts` file to verify against.
    known_hosts: PathBuf,
}

impl ConnectHooks {
    /// The hooks for one attempt.
    pub fn new(
        events: tokio::sync::mpsc::Sender<RemoteEvent>,
        attempt: ConnectId,
        known_hosts: PathBuf,
    ) -> Self {
        Self {
            events,
            attempt: attempt.0,
            known_hosts,
        }
    }

    /// This attempt's id.
    fn attempt(&self) -> ConnectId {
        ConnectId(self.attempt)
    }

    /// Show the fingerprint and ask.
    ///
    /// `false` for every way of not saying yes: a `Cancel`, a dismissed
    /// dialog, a dropped reply channel, or an event loop that has already gone
    /// away. Never defaults to accepting.
    async fn ask_host_key(&self, target: &Target, fingerprint: &str) -> bool {
        let (reply, answer) = tokio::sync::oneshot::channel();
        let event = RemoteEvent::HostKey {
            attempt: self.attempt(),
            target: target.clone(),
            fingerprint: fingerprint.to_string(),
            reply,
        };
        if self.events.send(event).await.is_err() {
            return false;
        }
        matches!(answer.await, Ok(true))
    }

    /// Say that the key changed. There is no answer to this, by design.
    async fn report_changed(&self, target: &Target, fingerprint: &str, line: usize, file: &Path) {
        let event = RemoteEvent::HostKeyChanged {
            attempt: self.attempt(),
            target: target.clone(),
            fingerprint: fingerprint.to_string(),
            line,
            file: file.to_path_buf(),
        };
        let _ = self.events.send(event).await;
    }

    /// Ask for a password or a passphrase.
    async fn ask_secret(&self, kind: SecretKind, offer_keyring: bool) -> Option<SecretAnswer> {
        let (reply, answer) = tokio::sync::oneshot::channel();
        let event = RemoteEvent::Secret {
            attempt: self.attempt(),
            kind,
            offer_keyring,
            reply,
        };
        if self.events.send(event).await.is_err() {
            return None;
        }
        answer.await.ok().flatten()
    }
}

impl fmt::Debug for ConnectHooks {
    /// The attempt and the file. Never the channel, which could otherwise be
    /// walked to a pending secret.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConnectHooks")
            .field("attempt", &self.attempt)
            .field("known_hosts", &self.known_hosts)
            .finish()
    }
}

/// The russh handler: host-key verification and nothing else.
struct ClientHandler {
    target: Target,
    hooks: ConnectHooks,
    /// Cleared when russh reports the session gone, which is what
    /// [`SftpFs::is_live`] reads (the disconnected state).
    live: Arc<AtomicBool>,
    /// Set while a host-key dialog is on screen, so the connect timeout does
    /// not count the time a human takes to answer.
    ///
    asking: Arc<AtomicBool>,
    /// Why the key was refused, for the message the user sees. Never a secret;
    /// a host key is public by definition.
    refusal: Arc<Mutex<Option<String>>>,
}

impl ClientHandler {
    /// Record why a key was refused, keeping the first reason.
    fn refuse(&self, reason: String) {
        if let Ok(mut slot) = self.refusal.lock()
            && slot.is_none()
        {
            *slot = Some(reason);
        }
    }
}

impl russh::client::Handler for ClientHandler {
    type Error = russh::Error;

    /// and the only place in the program that decides whether a
    /// host is the host it claims to be.
    ///
    /// Four outcomes and no fifth, because [`Verdict`] has four.
    /// `Changed` and `Revoked` return `false`
    /// and go nowhere near [`known_hosts::learn`]; there is no argument, no
    /// configuration key and no environment variable that changes that.
    async fn check_server_key(
        &mut self,
        key: &PublicKeyOrCertificate,
    ) -> std::result::Result<bool, Self::Error> {
        let key = match key {
            PublicKeyOrCertificate::PublicKey { key, .. } => key.clone(),
            // An OpenSSH certificate is a different trust model - it is
            // checked against a `@cert-authority` line and its principals and
            // validity window, none of which this milestone implements. The
            // safe answer to a question we cannot ask properly is no.
            PublicKeyOrCertificate::Certificate(_) => {
                self.refuse(format!(
                    "{} presented an OpenSSH certificate; this version verifies plain host keys only",
                    self.target.host
                ));
                return Ok(false);
            }
        };
        let file = self.hooks.known_hosts.clone();
        // Unbracketed: `lookup_name` writes `[host]:port` itself, and
        // `[[::1]]:2222` matches nothing `ssh` ever wrote.
        let verdict =
            match known_hosts::verify(&file, self.target.hostname(), self.target.port, &key) {
                Ok(verdict) => verdict,
                Err(err) => {
                    // A `known_hosts` that cannot be read is not permission to
                    // trust anything.
                    self.refuse(format!("{}: {err}", file.display()));
                    return Ok(false);
                }
            };
        match verdict {
            Verdict::Known => Ok(true),
            Verdict::Unknown { fingerprint } => {
                self.asking.store(true, Ordering::Release);
                let accepted = self.hooks.ask_host_key(&self.target, &fingerprint).await;
                self.asking.store(false, Ordering::Release);
                if !accepted {
                    self.refuse("the host key was not accepted".to_string());
                    return Ok(false);
                }
                // The one call site of `learn` in the tree, and it is behind
                // the user having seen the fingerprint and said yes
                // (invariant S6).
                if let Err(err) =
                    known_hosts::learn(&file, self.target.hostname(), self.target.port, &key)
                {
                    // The connection is still honest: the user accepted this
                    // key. Not recording it means being asked again.
                    self.refuse(format!("{}: {err}", file.display()));
                }
                Ok(true)
            }
            Verdict::Changed { line, fingerprint } => {
                self.hooks
                    .report_changed(&self.target, &fingerprint, line, &file)
                    .await;
                self.refuse(format!(
                    "the host key for {} has changed; {} line {line} records a different key",
                    self.target.host,
                    file.display()
                ));
                Ok(false)
            }
            Verdict::Revoked { line } => {
                let fingerprint = known_hosts::fingerprint(&key);
                self.hooks
                    .report_changed(&self.target, &fingerprint, line, &file)
                    .await;
                self.refuse(format!(
                    "the host key for {} is marked revoked at {} line {line}",
                    self.target.host,
                    file.display()
                ));
                Ok(false)
            }
        }
    }

    /// The link went. Everything above notices through [`SftpFs::is_live`].
    async fn disconnected(
        &mut self,
        reason: russh::client::DisconnectReason<Self::Error>,
    ) -> std::result::Result<(), Self::Error> {
        self.live.store(false, Ordering::Release);
        match reason {
            russh::client::DisconnectReason::ReceivedDisconnect(_) => Ok(()),
            russh::client::DisconnectReason::Error(err) => Err(err),
        }
    }
}

/// the `SftpFs`: russh plus russh-sftp behind [`RemoteTransport`].
///
/// Holds no session, no socket and no secret: one sender to the connection
/// actor and one flag. That is what makes its [`fmt::Debug`] safe by
/// construction rather than by redaction.
pub struct SftpFs {
    /// Where this connection points. Secret-free by construction.
    ///
    target: Target,
    /// The actor's end of the command channel.
    tx: UnboundedSender<Command>,
    /// False once the connection is gone.
    ///
    /// the design shows a plain `AtomicBool`; it is shared
    /// here because the flag has three writers - this type, the actor and
    /// russh's own handler - and a flag only one of them could set would be
    /// wrong exactly when it matters.
    live: Arc<AtomicBool>,
}

impl SftpFs {
    /// Connect, verify the host key, authenticate, and open the pool.
    ///
    ///
    /// **Async**, and the only async entry point in this module. It runs as a
    /// `tokio::spawn`ed task owned by the event loop, because it has to be
    /// able to wait for a dialog while the UI keeps drawing.
    ///
    ///
    /// The order is not negotiable and is the order of the code below:
    /// TCP, then the SSH handshake **including host-key verification**, then
    /// authentication (invariant S7). `password` is a password typed on a
    /// quick-connect line; it is used for this connection and nothing else,
    /// and it is never written anywhere.
    pub async fn connect(
        target: Target,
        plan: AuthPlan,
        password: Option<Secret>,
        config: &RemoteConfig,
        store: Arc<dyn SecretStore>,
        hooks: ConnectHooks,
    ) -> Result<Arc<Self>> {
        let authority = target.authority();
        let live = Arc::new(AtomicBool::new(true));
        let asking = Arc::new(AtomicBool::new(false));
        let refusal: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

        let ssh = Arc::new(russh::client::Config {
            // the liveness, handed to the library that is already
            // counting: `remote.keepalive` apart, `KEEPALIVE_MISSES` missed.
            keepalive_interval: Some(config.keepalive.0),
            keepalive_max: KEEPALIVE_MISSES,
            // No inactivity timeout: an idle panel on a remote is a normal
            // state and closing it under the user is not.
            inactivity_timeout: None,
            nodelay: true,
            ..russh::client::Config::default()
        });

        // DNS and TCP, under `remote.connect_timeout`.
        //
        let socket = tokio::time::timeout(
            config.connect_timeout.0,
            // `Target::hostname`, not `Target::host`: the parser keeps an IPv6
            // literal's brackets and `to_socket_addrs` will
            // not take them.
            TcpStream::connect((target.hostname(), target.port)),
        )
        .await
        .map_err(|_| timed_out(&authority, config.connect_timeout.0))?
        .map_err(|err| Error::msg(format!("{authority}: {err}")))?;

        let handler = ClientHandler {
            target: target.clone(),
            hooks: hooks.clone(),
            live: Arc::clone(&live),
            asking: Arc::clone(&asking),
            refusal: Arc::clone(&refusal),
        };
        let mut handle = handshake(
            ssh,
            socket,
            handler,
            config.connect_timeout.0,
            &asking,
            &authority,
            &refusal,
        )
        .await?;

        authenticate(&mut handle, &target, plan, password, store.as_ref(), &hooks).await?;

        let (pool, limits) = open_pool(&handle, config.pool_size, &authority).await?;
        let depth = config.pipeline.clamp(1, 64);
        let window = crate::ops::chunk_size(&Capabilities::SFTP)
            .min(limits.read)
            .clamp(1, MAX_WINDOW);
        let context = Arc::new(Context {
            authority,
            limits,
            depth,
            window,
            live: Arc::clone(&live),
        });
        let (tx, rx) = unbounded_channel::<Command>();
        let pool = Arc::new(pool);
        tokio::spawn(actor::run(rx, pool, context, handle));

        Ok(Arc::new(Self { target, tx, live }))
    }

    /// Where this connection points.
    pub fn target(&self) -> &Target {
        &self.target
    }

    /// The directory the server put us in, or the one the target named
    /// (the "wherever the server puts us").
    ///
    /// **Blocking.** Call from the blocking pool only.
    ///
    pub fn start_dir(&self) -> Result<String> {
        let wanted = self.target.dir.clone().unwrap_or_else(|| ".".to_string());
        self.realpath(&wanted)
    }

    /// `SSH_FXP_REALPATH`: a relative or `~`-shaped path made absolute by the
    /// server, which is the only thing that knows.
    ///
    /// **Blocking.** Call from the blocking pool only.
    ///
    pub fn realpath(&self, path: &str) -> Result<String> {
        let path = attrs::normalise(path);
        self.ask(|reply| Command::RealPath {
            path: path.clone(),
            reply,
        })
    }

    /// Set a file's mode bits (the "mode ... preserved").
    ///
    /// Not on [`RemoteTransport`], because FTP has no mode bits to set and
    /// the design fixes that trait's method list. It is here
    /// so that the preservation the design asks for is available on the
    /// backend that can actually do it, and so that the copy engine has
    /// something to call when it grows the remote half.
    ///
    /// **Blocking.** Call from the blocking pool only.
    ///
    pub fn set_permissions(&self, path: &str, mode: u32) -> Result<()> {
        let path = attrs::normalise(path);
        self.ask(|reply| Command::SetPermissions {
            path: path.clone(),
            mode,
            reply,
        })
    }

    /// Send one command and wait for its answer.
    ///
    /// The whole of the blocking bridge. A failure to send and a failure to
    /// receive are the same thing - the actor is gone - and both are
    /// [`Error::ConnectionLost`], which is what stops a batch rather than
    /// failing two hundred files identically.
    ///
    fn ask<T>(&self, build: impl FnOnce(Reply<T>) -> Command) -> Result<T> {
        // Depth one: this thread parks on `recv` and has exactly one command
        // in flight.
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        if self.tx.send(build(tx)).is_err() {
            return Err(self.lost());
        }
        match rx.recv() {
            Ok(answer) => answer,
            Err(_) => Err(self.lost()),
        }
    }

    /// Mark the connection gone and say so.
    fn lost(&self) -> Error {
        self.live.store(false, Ordering::Release);
        Error::ConnectionLost(self.target.authority())
    }
}

/// The connect timeout, phrased for a status line.
fn timed_out(authority: &str, limit: Duration) -> Error {
    Error::msg(format!(
        "{authority}: no answer within {} seconds",
        limit.as_secs()
    ))
}

/// The SSH handshake, with the timeout paused while a human is answering.
///
/// the design puts DNS, TCP and the handshake under
/// `remote.connect_timeout` and then says the timeout must not cover anything
/// that waits for a person. Host-key verification happens *inside* the
/// handshake, so both rules can only hold together if the clock stops while
/// the dialog is up. That is what `asking` is for.
async fn handshake(
    ssh: Arc<russh::client::Config>,
    socket: TcpStream,
    handler: ClientHandler,
    limit: Duration,
    asking: &Arc<AtomicBool>,
    authority: &str,
    refusal: &Arc<Mutex<Option<String>>>,
) -> Result<Handle<ClientHandler>> {
    let connecting = russh::client::connect_stream(ssh, socket, handler);
    tokio::pin!(connecting);
    loop {
        tokio::select! {
            done = &mut connecting => {
                return match done {
                    Ok(handle) => Ok(handle),
                    Err(err) => Err(handshake_error(authority, refusal, &err)),
                };
            }
            () = tokio::time::sleep(limit) => {
                if asking.load(Ordering::Acquire) {
                    // A dialog is on screen. The user's thinking time is not
                    // the network's fault.
                    continue;
                }
                return Err(timed_out(authority, limit));
            }
        }
    }
}

/// What to say when the handshake failed.
///
/// A refused host key is the interesting case and russh reports it as a
/// generic failure, so the reason recorded by [`ClientHandler::check_server_key`]
/// wins when there is one. A host key is public, so none of this can leak a
/// secret.
fn handshake_error(
    authority: &str,
    refusal: &Arc<Mutex<Option<String>>>,
    err: &russh::Error,
) -> Error {
    let reason = refusal
        .lock()
        .ok()
        .and_then(|slot| slot.clone())
        .unwrap_or_else(|| err.to_string());
    Error::msg(format!("{authority}: {reason}"))
}

/// the order, walked.
///
/// The plan is built elsewhere and is already in the right order; this drives
/// it and never reorders it. A secret is asked for at most [`MAX_ASKS`] times
/// per attempt, so a server that rejects everything cannot loop.
///
async fn authenticate(
    handle: &mut Handle<ClientHandler>,
    target: &Target,
    plan: AuthPlan,
    typed: Option<Secret>,
    store: &dyn SecretStore,
    hooks: &ConnectHooks,
) -> Result<bool> {
    let mut sequence = super::auth::AuthSequence::new(plan);
    // The password typed on a quick-connect line, used once and never stored.
    //
    let mut typed = typed;
    // The answer to the question currently on screen.
    let mut supplied: Option<Secret> = None;
    // Whether a password actually reached the keyring, which is the other half
    // of the opt-in: the caller records it on the host, or nothing
    // will ever read the secret back.
    let mut saved = false;
    let mut remember = false;
    let account = target.keyring_account();

    while !sequence.accepted() {
        let Some(method) = sequence.peek().cloned() else {
            break;
        };
        let outcome = match &method {
            Method::Agent => try_agent(handle, &target.user).await,
            Method::Key { path } => try_key(handle, &target.user, path, supplied.as_ref()).await,
            Method::Password => {
                let offered = supplied.take().or_else(|| typed.take());
                match offered {
                    None => Outcome::Needs(SecretKind::Password {
                        authority: target.authority(),
                    }),
                    Some(secret) => {
                        let accepted = try_password(handle, target, &secret).await?;
                        if accepted {
                            // Only now, and never before: a typo must not be
                            // what gets stored.
                            if remember && store.available() {
                                // A keyring that refuses is not a reason to
                                // hang up on a connection the server has
                                // already accepted: the user is connected,
                                // and the only cost is being asked again next
                                // time. There is no event in
                                // the design for saying so,
                                // and inventing one here would be this
                                // module deciding another module's contract.
                                saved = store.set(&account, &secret).is_ok();
                            }
                            Outcome::Accepted
                        } else {
                            retry_or_reject(
                                &sequence,
                                SecretKind::Password {
                                    authority: target.authority(),
                                },
                            )
                        }
                    }
                }
            }
            Method::Stored => try_stored(handle, target, store, &account).await,
        };

        match outcome {
            Outcome::Needs(kind) => {
                sequence.record(Outcome::Needs(kind.clone()));
                if sequence.asks() > MAX_ASKS {
                    // `record` counts the asks and, past the limit, advances
                    // past this method with its own explanation. Asking a
                    // fourth time would be the loop the design does not
                    // want.
                    supplied = None;
                    continue;
                }
                // The checkbox is offered only where there is a keyring to
                // put it in, and never for a key passphrase: storing one of
                // those would be inventing a policy the design has not got.
                //
                let offer = matches!(kind, SecretKind::Password { .. }) && store.available();
                match hooks.ask_secret(kind, offer).await {
                    // `Esc`, or a dropped reply channel. A cancelled connect
                    // is not a failed one and says so.
                    None => return Err(Error::Cancelled),
                    Some(answer) => {
                        remember = answer.remember;
                        supplied = Some(answer.secret);
                    }
                }
            }
            Outcome::Accepted => {
                supplied = None;
                sequence.record(Outcome::Accepted);
            }
            other => {
                supplied = None;
                sequence.record(other);
            }
        }
    }

    if sequence.accepted() {
        Ok(saved)
    } else {
        // Every method that was tried and why. Built by the state machine, so
        // it can carry nothing this function had in its hands.
        Err(Error::msg(sequence.failure_message(target)))
    }
}

/// Ask again, or give up, depending on how many times we have asked.
fn retry_or_reject(sequence: &super::auth::AuthSequence, kind: SecretKind) -> Outcome {
    if sequence.asks() < MAX_ASKS {
        // `Outcome::Needs` does not advance the sequence, which is what makes
        // a mistyped password re-askable rather than a fall-through to the
        // next method.
        Outcome::Needs(kind)
    } else {
        Outcome::Rejected
    }
}

/// Method 1: the SSH agent. Tried first, always.
async fn try_agent(handle: &mut Handle<ClientHandler>, user: &str) -> Outcome {
    let mut agent = match russh::keys::agent::client::AgentClient::connect_env().await {
        Ok(agent) => agent,
        Err(_) => {
            return Outcome::Unavailable(
                "no SSH agent is reachable (SSH_AUTH_SOCK is unset or the socket is gone)"
                    .to_string(),
            );
        }
    };
    let identities = match agent.request_identities().await {
        Ok(identities) => identities,
        Err(err) => {
            return Outcome::Unavailable(format!("the SSH agent refused to list keys: {err}"));
        }
    };
    if identities.is_empty() {
        return Outcome::Unavailable("the SSH agent holds no keys".to_string());
    }
    for identity in identities {
        let russh::keys::agent::AgentIdentity::PublicKey { key, .. } = identity else {
            // A certificate identity is a different authentication method and
            // is not offered here; see `check_server_key`.
            continue;
        };
        let hash = rsa_hash(handle, &key).await;
        if let Ok(AuthResult::Success) = handle
            .authenticate_publickey_with(user, key, hash, &mut agent)
            .await
        {
            return Outcome::Accepted;
        }
    }
    Outcome::Rejected
}

/// Method 2: a key file, with a passphrase prompt when it is encrypted.
///
async fn try_key(
    handle: &mut Handle<ClientHandler>,
    user: &str,
    path: &Path,
    passphrase: Option<&Secret>,
) -> Outcome {
    let text = match tokio::fs::read_to_string(path).await {
        Ok(text) => text,
        Err(err) => {
            return Outcome::Unavailable(format!("{}: {err}", path.display()));
        }
    };
    // The passphrase is borrowed for exactly the length of this call and is
    // never copied into an owned `String`.
    let word = match passphrase {
        Some(secret) => match secret.expose_str() {
            Some(text) => Some(text),
            None => {
                return Outcome::Unavailable(
                    "the passphrase is not valid UTF-8 and cannot be used".to_string(),
                );
            }
        },
        None => None,
    };
    let key = match russh::keys::decode_secret_key(&text, word) {
        Ok(key) => key,
        Err(russh::keys::Error::KeyIsEncrypted) => {
            return Outcome::Needs(SecretKind::Passphrase {
                key: path.to_path_buf(),
            });
        }
        Err(err) => {
            if word.is_some() {
                // A passphrase that did not open the key. Asking again is the
                // right answer and `Needs` is what does not advance.
                return Outcome::Needs(SecretKind::Passphrase {
                    key: path.to_path_buf(),
                });
            }
            return Outcome::Unavailable(format!("{}: {err}", path.display()));
        }
    };
    let key = Arc::new(key);
    let hash = if key.algorithm().is_rsa() {
        best_rsa_hash(handle).await
    } else {
        None
    };
    match handle
        .authenticate_publickey(user, PrivateKeyWithHashAlg::new(key, hash))
        .await
    {
        Ok(AuthResult::Success) => Outcome::Accepted,
        Ok(AuthResult::Failure { .. }) => Outcome::Rejected,
        Err(err) => Outcome::Unavailable(err.to_string()),
    }
}

/// Method 3: a password, held only for this connection.
///
/// Returns whether the server accepted it. A transport failure here is fatal
/// rather than "the next method": the socket is gone.
async fn try_password(
    handle: &mut Handle<ClientHandler>,
    target: &Target,
    secret: &Secret,
) -> Result<bool> {
    let Some(word) = secret.expose_str() else {
        return Ok(false);
    };
    match handle.authenticate_password(&target.user, word).await {
        Ok(AuthResult::Success) => Ok(true),
        Ok(AuthResult::Failure { .. }) => Ok(false),
        // The socket is gone, and that is `ConnectionLost` rather than a
        // sentence about it: an `Error::msg` here erased the one thing a
        // caller can act on, and the russh text goes in the authority's place
        // where nothing reads it as a classification. It quotes no credential
        // either way - russh sends the password and reports only what came
        // back.
        Err(err) => Err(Error::connection_lost(format!(
            "{}: {err}",
            target.authority()
        ))),
    }
}

/// Method 4: the stored password, and only where the user opted in per host.
///
async fn try_stored(
    handle: &mut Handle<ClientHandler>,
    target: &Target,
    store: &dyn SecretStore,
    account: &str,
) -> Outcome {
    if !store.available() {
        // say so and fall back to prompting every time. Never a
        // silent write to disk, which `NoKeyring` makes impossible anyway.
        return Outcome::Unavailable(keyring::unavailable_message());
    }
    let secret = match store.get(account) {
        Ok(Some(secret)) => secret,
        Ok(None) => {
            return Outcome::Unavailable(format!(
                "nothing is stored in the keyring for {}",
                target.authority()
            ));
        }
        Err(err) => return Outcome::Unavailable(err.to_string()),
    };
    match try_password(handle, target, &secret).await {
        Ok(true) => Outcome::Accepted,
        // A stored password the server rejects is stale. Falling through to
        // the prompt is what lets the user replace it; asking the keyring
        // again would only get the same answer.
        Ok(false) => Outcome::Rejected,
        Err(err) => Outcome::Unavailable(err.to_string()),
    }
}

/// The RSA hash the server prefers, or `None` for a key that is not RSA.
async fn rsa_hash(handle: &Handle<ClientHandler>, key: &PublicKey) -> Option<russh::keys::HashAlg> {
    if matches!(key.algorithm(), Algorithm::Rsa { .. }) {
        best_rsa_hash(handle).await
    } else {
        None
    }
}

/// `rsa-sha2-512` where the server supports it, and SHA-1 where it does not.
async fn best_rsa_hash(handle: &Handle<ClientHandler>) -> Option<russh::keys::HashAlg> {
    handle
        .best_supported_rsa_hash()
        .await
        .ok()
        .flatten()
        .flatten()
}

/// Open `remote.pool_size` SFTP channels on the one SSH connection.
///
///
/// They are channels, not connections: SSH multiplexes, and one
/// authentication is what the user asked for when they connected once.
///
async fn open_pool(
    handle: &Handle<ClientHandler>,
    size: usize,
    authority: &str,
) -> Result<(Pool, WireLimits)> {
    let size = size.clamp(1, 16);
    let mut sessions = Vec::with_capacity(size);
    let mut limits = WireLimits::default();
    for _ in 0..size {
        let channel = handle
            .channel_open_session()
            .await
            .map_err(|err| Error::msg(format!("{authority}: {err}")))?;
        channel
            .request_subsystem(true, "sftp")
            .await
            .map_err(|err| Error::msg(format!("{authority}: the sftp subsystem: {err}")))?;
        let mut session = russh_sftp::client::RawSftpSession::new(channel.into_stream());
        let version = session
            .init()
            .await
            .map_err(|err| Error::msg(format!("{authority}: {err}")))?;
        // `limits@openssh.com` is what turns the 32 KiB every server must
        // accept into the 256 KiB OpenSSH will (the throughput).
        if version
            .extensions
            .get(russh_sftp::extensions::LIMITS)
            .is_some_and(|value| value == "1")
            && let Ok(announced) = session.limits().await
        {
            limits = WireLimits {
                read: payload(announced.max_read_len),
                write: payload(announced.max_write_len),
            };
            session.set_limits(announced.into());
        }
        sessions.push(Arc::new(session));
    }
    Ok((Pool::new(sessions), limits))
}

/// One announced limit, floored at what every server must accept and capped at
/// what this program is willing to hold in memory.
fn payload(announced: u64) -> usize {
    let announced = usize::try_from(announced).unwrap_or(SAFE_PAYLOAD);
    if announced == 0 {
        SAFE_PAYLOAD
    } else {
        announced.clamp(SAFE_PAYLOAD, MAX_WINDOW)
    }
}

impl RemoteTransport for SftpFs {
    fn protocol(&self) -> Protocol {
        Protocol::Sftp
    }

    /// The fixed answer for SFTP. Never does I/O, because
    /// `App::apply_vfs_event` asks on the event loop.
    ///
    fn capabilities(&self) -> Capabilities {
        Capabilities::SFTP
    }

    /// **Blocking.** Call from the blocking pool only.
    ///
    fn list(&self, dir: &str) -> Result<Vec<Entry>> {
        let dir = attrs::normalise(dir);
        self.ask(|reply| Command::List {
            dir: dir.clone(),
            reply,
        })
    }

    /// **Blocking.** Call from the blocking pool only.
    ///
    fn stat(&self, path: &str) -> Result<Entry> {
        let path = attrs::normalise(path);
        self.ask(|reply| Command::Stat {
            path: path.clone(),
            reply,
        })
    }

    /// **Blocking.** Call from the blocking pool only.
    ///
    fn read_link(&self, path: &str) -> Result<String> {
        let path = attrs::normalise(path);
        self.ask(|reply| Command::ReadLink {
            path: path.clone(),
            reply,
        })
    }

    /// **Blocking.** Call from the blocking pool only.
    ///
    fn open_read(&self, path: &str) -> Result<Box<dyn std::io::Read + Send>> {
        let path = attrs::normalise(path);
        let reader: PipelinedReader = self.ask(|reply| Command::OpenRead {
            path: path.clone(),
            reply,
        })?;
        Ok(Box::new(reader))
    }

    /// **Blocking.** Call from the blocking pool only.
    ///
    fn open_seek(&self, path: &str) -> Result<Box<dyn ReadSeek + Send>> {
        let path = attrs::normalise(path);
        let reader = self.ask(|reply| Command::OpenSeek {
            path: path.clone(),
            reply,
        })?;
        Ok(Box::new(reader))
    }

    /// **Blocking.** Call from the blocking pool only.
    ///
    fn open_write(&self, path: &str) -> Result<Box<dyn std::io::Write + Send>> {
        let path = attrs::normalise(path);
        let writer = self.ask(|reply| Command::OpenWrite {
            path: path.clone(),
            reply,
        })?;
        Ok(Box::new(writer))
    }

    /// **Blocking.** Call from the blocking pool only.
    ///
    fn create_dir(&self, path: &str) -> Result<()> {
        let path = attrs::normalise(path);
        self.ask(|reply| Command::Mkdir {
            path: path.clone(),
            reply,
        })
    }

    /// **Blocking.** Call from the blocking pool only.
    ///
    fn remove_file(&self, path: &str) -> Result<()> {
        let path = attrs::normalise(path);
        self.ask(|reply| Command::RemoveFile {
            path: path.clone(),
            reply,
        })
    }

    /// **Blocking.** Call from the blocking pool only.
    ///
    fn remove_dir(&self, path: &str) -> Result<()> {
        let path = attrs::normalise(path);
        self.ask(|reply| Command::RemoveDir {
            path: path.clone(),
            reply,
        })
    }

    /// **Blocking.** Call from the blocking pool only.
    ///
    fn rename(&self, from: &str, to: &str) -> Result<()> {
        let from = attrs::normalise(from);
        let to = attrs::normalise(to);
        self.ask(|reply| Command::Rename {
            from: from.clone(),
            to: to.clone(),
            reply,
        })
    }

    /// Never does I/O: it reads a flag the actor and russh's handler set.
    /// The panel asks this on every frame.
    fn is_live(&self) -> bool {
        self.live.load(Ordering::Acquire) && !self.tx.is_closed()
    }

    /// Close every channel and stop the actor. Idempotent.
    fn close(&self) {
        self.live.store(false, Ordering::Release);
        let _ = self.tx.send(Command::Close);
    }
}

impl fmt::Debug for SftpFs {
    /// Target and liveness. Never the session, never the pool, and there is no
    /// secret in this type to leave out.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SftpFs")
            .field("target", &self.target)
            .field("live", &self.live.load(Ordering::Acquire))
            .finish()
    }
}

impl Drop for SftpFs {
    /// Close, so a dropped backend does not leave a socket and a task behind.
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pipeline_and_keepalive_constants_are_the_contracts() {
        assert_eq!(PIPELINE_DEPTH, 4, "the design");
        assert_eq!(KEEPALIVE_MISSES, 3, "the design");
    }

    #[test]
    fn capabilities_are_the_fixed_sftp_answer_and_need_no_connection() {
        // Invariant I5: `capabilities_for` must answer with the socket dead,
        // which is only possible because the answer is a constant.
        let caps = Capabilities::SFTP;
        assert!(caps.writable);
        assert!(caps.seekable, "SSH_FXP_READ takes an offset");
        assert!(!caps.random_access, "a round trip per window is not cheap");
        assert!(caps.atomic_rename);
        assert!(caps.paged_listing);
        assert!(!caps.can_execute);
        assert_eq!(caps.latency, crate::vfs::LatencyClass::Network);
    }

    #[test]
    fn the_read_window_never_exceeds_what_the_server_announced() {
        // 32 KiB when the server announces nothing.
        assert_eq!(payload(0), SAFE_PAYLOAD);
        // A server that announces less than the mandatory floor is not
        // believed downwards; the floor is the protocol's, not the server's.
        assert_eq!(payload(1024), SAFE_PAYLOAD);
        assert_eq!(payload(262_144), 262_144);
        // ...and an absurd announcement is capped rather than allocated.
        assert_eq!(payload(u64::MAX), MAX_WINDOW);
    }

    #[test]
    fn the_transfer_window_comes_from_capabilities_and_not_from_a_constant() {
        // Obligation 10 of the chunk size is
        // `ops::chunk_size(&caps)`, capped by the wire limit.
        let big = crate::ops::chunk_size(&Capabilities::SFTP)
            .min(payload(262_144))
            .clamp(1, MAX_WINDOW);
        assert_eq!(big, 262_144);
        let small = crate::ops::chunk_size(&Capabilities::SFTP)
            .min(payload(0))
            .clamp(1, MAX_WINDOW);
        assert_eq!(small, SAFE_PAYLOAD);
        assert!(
            crate::ops::chunk_size(&Capabilities::SFTP)
                > crate::ops::chunk_size(&Capabilities::LOCAL),
            "a network backend copies in larger chunks than a local one (I12)"
        );
    }

    #[test]
    fn a_timeout_message_names_the_connection_and_the_limit() {
        let err = timed_out("sftp://thorin@nas.local:2222", Duration::from_secs(10));
        assert_eq!(
            err.to_string(),
            "sftp://thorin@nas.local:2222: no answer within 10 seconds"
        );
    }

    #[test]
    fn a_handshake_failure_prefers_the_reason_the_key_check_recorded() {
        let refusal = Arc::new(Mutex::new(Some(
            "the host key was not accepted".to_string(),
        )));
        let err = handshake_error("sftp://t@h:22", &refusal, &russh::Error::Disconnect);
        assert_eq!(
            err.to_string(),
            "sftp://t@h:22: the host key was not accepted"
        );
        // ...and falls back to russh's own words when it recorded none.
        let silent = Arc::new(Mutex::new(None));
        let err = handshake_error("sftp://t@h:22", &silent, &russh::Error::Disconnect);
        assert!(err.to_string().starts_with("sftp://t@h:22: "));
        assert!(!err.to_string().ends_with(": "));
    }

    #[test]
    fn the_hooks_debug_shows_the_attempt_and_no_channel() {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let hooks = ConnectHooks::new(tx, ConnectId(7), PathBuf::from("/home/t/.ssh/known_hosts"));
        let shown = format!("{hooks:?}");
        assert!(shown.contains("attempt: 7"), "{shown}");
        assert!(shown.contains("known_hosts"), "{shown}");
        assert_eq!(hooks.attempt(), ConnectId(7));
    }
}
