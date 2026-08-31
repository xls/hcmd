//! Remote connections: SFTP, FTP/FTPS and SMB behind the ordinary [`Vfs`]
//! trait.
//!
//!
//! `Ctrl+F` connects the active panel in place; the panel then behaves exactly
//! like a local one, because "all of it goes through the same `Vfs` trait, so
//! no operation needs to know whether it is local or remote".
//!
//! # The async boundary, decided once
//!
//! > The async half of this milestone is confined to one tokio task per SSH
//! > connection. Everything above it is synchronous, and every synchronous
//! > call into it happens on the blocking pool.
//!
//! Concretely: [`sftp::SftpFs`] owns a connection actor - the only code in the
//! tree that `.await`s a socket - and its [`transport::RemoteTransport`]
//! methods are synchronous wrappers that send a command and block on the
//! reply. [`ftp::FtpFs`] has no actor at all: suppaftp's synchronous streams
//! are used on the calling blocking thread. [`smb::SmbFs`] has an actor like
//! SFTP's, because smb2 is async too. [`fs::RemoteFs`] is the single `Vfs`
//! implementation over any of them, so the mapping from `Vfs` to a remote
//! protocol is written once and the three protocols cannot drift apart.
//!
//! SMB is the one protocol here whose namespace is not a single tree: a share
//! is a tree connect, not a directory. It is represented as the **first
//! component of the connection's path**, which is written down in
//! [`smb`]'s own module documentation and is invisible above it.
//!
//! The connect **handshake** is the exception and is async by design: it has
//! to be able to sit waiting for a host-key or password dialog while the UI
//! keeps drawing, which is what [`crate::app::RemoteEvent`] and its `oneshot`
//! replies are for.
//!
//! # Secrets
//!
//! Nothing in this module's public surface carries one except [`secret::Secret`]
//! itself. [`Target`] is secret-free **by construction**, which is what makes
//! it safe to `Display` into a header, a tab title and a status line
//! (the design S3).

pub mod auth;
pub mod connect;
pub mod dav;
pub mod fs;
pub mod ftp;
pub mod hosts;
pub mod keyring;
pub mod known_hosts;
pub mod prompt;
pub mod secret;
pub mod sftp;
pub mod smb;
pub mod transport;
pub mod url;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::error::{Error, Result};
use crate::vfs::{BackendKind, Vfs, VfsPath};

pub use fs::RemoteFs;
pub use transport::RemoteTransport;

/// One live connection, by id.
///
/// Ids are never reused, so a stale path names nothing rather than someone
/// else's host - the rule [`crate::vfs::list::ListingId`] follows, and for the
/// same reason (the design I1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RemoteId(pub u32);

impl RemoteId {
    /// The path a panel sits at when it connects: the connection's id plus the
    /// remote directory, in the remote's own namespace.
    ///
    /// One segment, and the id is in the [`BackendKind`] rather than in the
    /// text: that is what makes [`VfsPath::starts_with`] answer `false`
    /// between two hosts holding identical paths (the design I2.
    pub fn path(self, dir: &str) -> VfsPath {
        let dir = if dir.is_empty() { "/" } else { dir };
        VfsPath::new(BackendKind::Remote(self), dir)
    }

    /// The root of that connection.
    pub fn root(self) -> VfsPath {
        self.path("/")
    }

    /// The id a path names, or `None` for anything that is not remote.
    pub fn from_path(path: &VfsPath) -> Option<Self> {
        // Every segment, not just the innermost. An archive on a remote is a
        // path whose outermost segment is the connection and whose innermost
        // is `Archive`, and asking only the innermost said "this is not on a
        // connection". `App::navigate` read that as leaving the remote, closed
        // the transport, and then could not read the archive whose container
        // lived on the transport it had just closed: an empty listing, "that
        // connection has been closed", and a panel with no `remote_view` left
        // to get home with.
        path.segments().iter().find_map(|(kind, _)| match kind {
            BackendKind::Remote(id) => Some(*id),
            // An image keeps its connection segment outermost, and that
            // segment is what this finds; an `Image` segment itself never
            // names a connection.
            BackendKind::Local | BackendKind::List | BackendKind::Archive | BackendKind::Image => {
                None
            }
        })
    }
}

impl std::fmt::Display for RemoteId {
    /// `remote:3`, for a message that has no [`Target`] at hand.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "remote:{}", self.0)
    }
}

/// Which protocol a connection speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Protocol {
    /// SFTP over SSH. The default and the primary target.
    Sftp,
    /// Plain FTP.
    Ftp,
    /// FTP with explicit TLS (`AUTH TLS` on the control port).
    Ftps,
    /// FTP with implicit TLS, historically port 990.
    FtpsImplicit,
    /// SMB2/SMB3, the protocol a Windows share and a NAS box speak.
    Smb,
    /// WebDAV over HTTP.
    Dav,
    /// WebDAV over HTTPS, which is how anybody sensible runs it.
    Davs,
}

impl Default for Protocol {
    /// SFTP, which is `[remote] default_protocol`'s shipped value and the one
    /// the design calls the primary target.
    fn default() -> Self {
        Self::Sftp
    }
}

impl Protocol {
    /// Every protocol, in the order the Add-host form cycles them.
    pub const ALL: &'static [Self] = &[
        Self::Sftp,
        Self::Ftp,
        Self::Ftps,
        Self::FtpsImplicit,
        Self::Smb,
        Self::Dav,
        Self::Davs,
    ];

    /// `"sftp"`, `"ftp"`, `"ftps"`, `"ftps-implicit"`: the `hosts.toml` value
    /// and the URL scheme in one string.
    pub const fn id(self) -> &'static str {
        match self {
            Self::Sftp => "sftp",
            Self::Ftp => "ftp",
            Self::Ftps => "ftps",
            Self::FtpsImplicit => "ftps-implicit",
            Self::Smb => "smb",
            Self::Dav => "dav",
            Self::Davs => "davs",
        }
    }

    /// 22, 21, 21, 990, 445, 80, 443.
    pub const fn default_port(self) -> u16 {
        match self {
            Self::Sftp => 22,
            Self::Ftp | Self::Ftps => 21,
            Self::FtpsImplicit => 990,
            Self::Smb => 445,
            Self::Dav => 80,
            Self::Davs => 443,
        }
    }

    /// The scheme as it is written in a header: `sftp`, `ftp`, `ftps`.
    ///
    /// Implicit FTPS writes `ftps` too, because that is what it speaks; the
    /// difference between the two is when the TLS handshake happens, which is
    /// a dialling detail and not something a header can usefully say.
    pub const fn scheme(self) -> &'static str {
        match self {
            Self::Sftp => "sftp",
            Self::Ftp => "ftp",
            Self::Ftps | Self::FtpsImplicit => "ftps",
            Self::Smb => "smb",
            Self::Dav => "dav",
            Self::Davs => "davs",
        }
    }

    /// Whether this protocol authenticates a host key.
    pub const fn verifies_host_key(self) -> bool {
        match self {
            Self::Sftp => true,
            // TLS verifies a certificate rather than a host key, which is a
            // different question with a different answer and a different
            // prompt: the trust store answers it, not the user.
            Self::Ftp | Self::Ftps | Self::FtpsImplicit | Self::Smb | Self::Dav | Self::Davs => {
                false
            }
        }
    }

    /// Parse a `hosts.toml` value or a URL scheme.
    ///
    /// `ssh` is an alias for `sftp`, because that is what a user types; and
    /// `ftps-implicit` is reachable from the host book and from a typed line
    /// alike, since refusing to parse a scheme this program itself writes
    /// would be a surprise.
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "sftp" | "ssh" => Some(Self::Sftp),
            "ftp" => Some(Self::Ftp),
            "ftps" => Some(Self::Ftps),
            "ftps-implicit" | "ftpsimplicit" => Some(Self::FtpsImplicit),
            // `cifs` is the same protocol under the name a mount table and a
            // decade of documentation still use for it.
            "smb" | "cifs" => Some(Self::Smb),
            // `http` and `https` are accepted as well as `dav` and `davs`,
            // because a WebDAV endpoint is nearly always written as the URL
            // somebody copied out of a browser or a Nextcloud settings page.
            // Nothing else in this program speaks HTTP to a panel, so there is
            // no other reading of those two.
            "dav" | "http" => Some(Self::Dav),
            "davs" | "https" | "webdav" => Some(Self::Davs),
            _ => None,
        }
    }
}

impl std::fmt::Display for Protocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.id())
    }
}

/// Where a connection points.
///
/// **Carries no secret**, which is what makes it safe to `Display` into a
/// header, a tab title and a status line (the design S3). There is
/// no field a password could be put in, so no formatting of this type can
/// carry one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// Which protocol to speak.
    pub protocol: Protocol,
    /// The host as it was typed: a name, an IPv4 literal, or a bracketed IPv6
    /// literal (`[::1]`).
    pub host: String,
    /// The port to dial. Already resolved from [`Protocol::default_port`] when
    /// the user did not type one.
    pub port: u16,
    /// The login name.
    pub user: String,
    /// The initial remote directory, or `None` for "wherever the server puts
    /// us", which SFTP and FTP both answer with the login directory.
    pub dir: Option<String>,
}

impl Target {
    /// The host as a socket and `known_hosts` want it: the bracketed IPv6
    /// literal the user typed, unbracketed.
    ///
    /// [`Target::host`] keeps the brackets, because that is the form the design
    /// asks the line to accept and the form the header prints. The resolver
    /// does not take it: `("[::1]", 22).to_socket_addrs()` fails with "Name or
    /// service not known", where `("::1", 22)` succeeds. Neither does
    /// `known_hosts`, whose lookup name for a non-default port is
    /// `[host]:port`: `ssh` writes `[::1]:2222`, and a doubly bracketed
    /// `[[::1]]:2222` would match no entry a user already has, turning every
    /// connection into an unknown-host prompt (a prompt the user learns to
    /// click through is a verification that has stopped working).
    ///
    /// One method rather than a `trim` at each socket, because there were two
    /// of those and only one of them was written.
    pub fn hostname(&self) -> &str {
        self.host
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
            .unwrap_or(&self.host)
    }

    /// `sftp://thorin@nas.local:2222` - no path, no password, ever.
    ///
    /// The port is always written, even when it is the default: this string is
    /// the keyring account, and a host reached on 22 and the same host reached
    /// on 2222 must not share a stored password.
    pub fn authority(&self) -> String {
        format!(
            "{}://{}@{}:{}",
            self.protocol.scheme(),
            self.user,
            self.host,
            self.port
        )
    }

    /// `sftp://thorin@nas.local:2222/srv/media` (the header).
    pub fn url(&self, dir: &str) -> String {
        let mut out = self.authority();
        if !dir.starts_with('/') {
            out.push('/');
        }
        out.push_str(dir);
        out
    }

    /// The keyring account this target is stored under.
    ///
    pub fn keyring_account(&self) -> String {
        self.authority()
    }
}

impl std::fmt::Display for Target {
    /// [`Target::authority`]. There is no formatting of a `Target` that can
    /// carry a secret, because a `Target` has none to carry.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.authority())
    }
}

/// At most this many connections at once.
///
/// Nine tabs on each of two panels, so the ceiling is
/// unreachable in normal use and is a refusal rather than an eviction when it
/// is not - the evicted one would be a panel somebody is looking at.
pub const MAX_CONNECTIONS: usize = 18;

/// The registry of live connections, owned by the router.
pub struct RemoteRegistry {
    /// A `std::sync::Mutex` and not a `RefCell`: the registry lives behind an
    /// `Arc` inside the router and is read from worker threads, so there is no
    /// `&mut` route to it at all.
    connections: std::sync::Mutex<HashMap<RemoteId, Arc<RemoteFs>>>,
    /// Monotonic source for [`RemoteId`]. Never reset, never reused.
    next: AtomicU32,
}

impl RemoteRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self {
            connections: std::sync::Mutex::new(HashMap::new()),
            next: AtomicU32::new(0),
        }
    }

    /// The registry lock, with a poisoned one recovered rather than escalated.
    ///
    /// What is behind it is a map of `Arc`s: a panicking thread cannot leave
    /// it half-written, and losing every open connection to one unrelated
    /// panic would be a worse answer than carrying on. The twin of
    /// `VfsRouter::listings`.
    fn map(&self) -> std::sync::MutexGuard<'_, HashMap<RemoteId, Arc<RemoteFs>>> {
        self.connections
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Register a connected backend and hand back the id its paths name.
    ///
    /// `Err` past [`MAX_CONNECTIONS`]; never a silent eviction, because the
    /// evicted one would be a panel somebody is looking at.
    pub fn register(&self, fs: Arc<RemoteFs>) -> Result<RemoteId> {
        let mut map = self.map();
        if map.len() >= MAX_CONNECTIONS {
            return Err(Error::msg(format!(
                "{MAX_CONNECTIONS} connections are already open; \
                 disconnect one with Ctrl+F first"
            )));
        }
        // `fetch_add` returns the previous value, so the first id is 1 and
        // `RemoteId(0)` is never a connection anyone registered.
        let id = RemoteId(self.next.fetch_add(1, Ordering::SeqCst).saturating_add(1));
        fs.adopt(id);
        map.insert(id, fs);
        Ok(id)
    }

    /// Put a fresh connection behind an id that is already in the map, which
    /// is what reconnecting a dropped connection does.
    ///
    /// The tab's path names the id, so replacing rather than registering is
    /// what makes `Ctrl+R` reconnect **without the panel moving**. The old
    /// backend is closed. `false` when the id is not registered, which is a
    /// connection the user has already disconnected.
    pub fn replace(&self, id: RemoteId, fs: Arc<RemoteFs>) -> bool {
        let mut map = self.map();
        if !map.contains_key(&id) {
            return false;
        }
        fs.adopt(id);
        if let Some(old) = map.insert(id, fs) {
            old.close();
        }
        true
    }

    /// The connection an id names, or `None` once it has been closed.
    pub fn get(&self, id: RemoteId) -> Option<Arc<RemoteFs>> {
        self.map().get(&id).map(Arc::clone)
    }

    /// Disconnect: close the transport and forget the id. Idempotent.
    pub fn close(&self, id: RemoteId) {
        let fs = self.map().remove(&id);
        if let Some(fs) = fs {
            fs.close();
        }
    }

    /// Ids in registration order, for the quit prompt and for tests.
    pub fn ids(&self) -> Vec<RemoteId> {
        let mut ids: Vec<RemoteId> = self.map().keys().copied().collect();
        ids.sort_unstable();
        ids
    }

    /// How many are open.
    pub fn count(&self) -> usize {
        self.map().len()
    }

    /// `sftp://thorin@nas.local:2222`, or `None` once closed.
    pub fn authority(&self, id: RemoteId) -> Option<String> {
        self.map().get(&id).map(|fs| fs.target().authority())
    }
}

impl Default for RemoteRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for RemoteRegistry {
    /// Close every connection, so the process does not exit with sockets and
    /// an actor task still live. The twin of `VfsRouter::drop`.
    fn drop(&mut self) {
        for (_, fs) in self.map().drain() {
            fs.close();
        }
    }
}

impl std::fmt::Debug for RemoteRegistry {
    /// Ids and authorities. No transport, no session, no secret.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let map = self.map();
        let mut ids: Vec<&RemoteId> = map.keys().collect();
        ids.sort_unstable();
        let rows: Vec<String> = ids
            .into_iter()
            .filter_map(|id| map.get(id).map(|fs| format!("{id}={}", fs.target())))
            .collect();
        f.debug_struct("RemoteRegistry")
            .field("connections", &rows)
            .finish()
    }
}

/// The registry as a `Vfs` lookup, for [`crate::vfs::VfsRouter::backend_for`].
///
/// A free function rather than a method so the router's arm reads the same as
/// its `List` one: the id, the lookup, and a message that says the connection
/// has been closed rather than listing `/`.
///
/// [`Error::ConnectionClosed`] rather than an [`Error::Msg`] carrying the same
/// sentence: a job whose connection was closed under it has to **stop**, and
/// [`crate::ops::is_fatal`] is where that is decided.
pub fn backend_for(
    registry: &RemoteRegistry,
    id: RemoteId,
    path: &VfsPath,
) -> Result<Arc<dyn Vfs>> {
    registry
        .get(id)
        .map(|fs| fs as Arc<dyn Vfs>)
        .ok_or_else(|| Error::ConnectionClosed(path.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_remote_path_carries_its_id_in_the_backend_not_in_the_text() {
        let three = RemoteId(3).path("/srv/media");
        let four = RemoteId(4).path("/srv/media");
        assert_eq!(three.to_string(), "/srv/media");
        assert_eq!(three.backend(), BackendKind::Remote(RemoteId(3)));
        // I2: identical text, different connections, so neither is beneath the
        // other.
        assert!(!three.starts_with(&four));
        assert!(!four.starts_with(&three));
        assert!(three.starts_with(&RemoteId(3).root()));
    }

    #[test]
    fn from_path_answers_only_for_a_remote_path() {
        assert_eq!(RemoteId::from_path(&RemoteId(7).root()), Some(RemoteId(7)));
        assert_eq!(RemoteId::from_path(&VfsPath::local("/etc")), None);
    }

    #[test]
    fn a_remote_path_is_never_handed_to_the_kernel() {
        // the design relies on this: a remote file has no local path, so
        // it cannot be executed.
        assert!(RemoteId(1).path("/bin/sh").local_path().is_none());
    }

    /// the design accepts `sftp://[::1]:2222` and the header prints it
    /// back with the brackets; `to_socket_addrs` will not take them.
    ///
    /// The two halves used to disagree - FTP trimmed the brackets at its own
    /// socket and SFTP did not, so a bracketed literal could be reached over
    /// FTP and not over SFTP at all. `Target::hostname` is the one place that
    /// answers it now. The `to_socket_addrs` calls here resolve literals in
    /// the standard library and reach no network.
    #[test]
    fn a_bracketed_ipv6_literal_is_unbracketed_for_the_socket() {
        use std::net::ToSocketAddrs as _;

        let target = Target {
            protocol: Protocol::Sftp,
            host: "[::1]".to_string(),
            port: 2222,
            user: "thorin".to_string(),
            dir: None,
        };
        assert_eq!(target.hostname(), "::1");
        assert!(
            (target.hostname(), target.port).to_socket_addrs().is_ok(),
            "the unbracketed literal is what a socket takes"
        );
        assert!(
            (target.host.as_str(), target.port)
                .to_socket_addrs()
                .is_err(),
            "the bracketed literal is not, which is the whole bug"
        );
        // The header still prints what the user typed.
        assert_eq!(target.authority(), "sftp://thorin@[::1]:2222");

        // A name and an IPv4 literal pass through untouched, brackets being
        // meaningful only around an IPv6 literal.
        for plain in ["nas.local", "192.168.1.10", "::1"] {
            let target = Target {
                host: plain.to_string(),
                ..target.clone()
            };
            assert_eq!(target.hostname(), plain);
        }
    }

    #[test]
    fn the_authority_always_carries_the_port() {
        let target = Target {
            protocol: Protocol::Sftp,
            host: "nas.local".to_string(),
            port: 2222,
            user: "thorin".to_string(),
            dir: Some("/srv/media".to_string()),
        };
        assert_eq!(target.authority(), "sftp://thorin@nas.local:2222");
        assert_eq!(
            target.url("/srv/media"),
            "sftp://thorin@nas.local:2222/srv/media"
        );
        assert_eq!(target.keyring_account(), target.authority());
        assert_eq!(target.to_string(), target.authority());
    }

    #[test]
    fn implicit_ftps_writes_ftps_in_a_header_and_ftps_implicit_in_a_file() {
        assert_eq!(Protocol::FtpsImplicit.scheme(), "ftps");
        assert_eq!(Protocol::FtpsImplicit.id(), "ftps-implicit");
        assert_eq!(Protocol::FtpsImplicit.default_port(), 990);
        assert_eq!(
            Protocol::parse("ftps-implicit"),
            Some(Protocol::FtpsImplicit)
        );
        assert_eq!(Protocol::parse("ssh"), Some(Protocol::Sftp));
        assert_eq!(Protocol::parse("http"), Some(Protocol::Dav));
        assert_eq!(Protocol::parse("https"), Some(Protocol::Davs));
        assert_eq!(Protocol::parse("dav"), Some(Protocol::Dav));
        assert_eq!(Protocol::parse("gopher"), None);
    }

    #[test]
    fn only_sftp_verifies_a_host_key() {
        assert!(Protocol::Sftp.verifies_host_key());
        for other in [
            Protocol::Ftp,
            Protocol::Ftps,
            Protocol::FtpsImplicit,
            Protocol::Dav,
            Protocol::Davs,
        ] {
            assert!(!other.verifies_host_key(), "{other} has no host key");
        }
    }
    #[test]
    fn an_archive_on_a_remote_is_still_on_that_connection() {
        // Reported from a real session: connect over SFTP, press Enter on a
        // zip, and the listing came up empty saying the connection had been
        // closed - with no way back to local, because the tab's remote_view
        // had been taken too. `App::navigate` asks this function whether the
        // path it is going to is still on the connection, and asking only the
        // innermost segment answered no for `sftp://host/x.zip#/`.
        use crate::vfs::BackendKind;
        let id = RemoteId(4);
        let inside = id
            .path("/srv/media/bundle.zip")
            .with_segment(BackendKind::Archive, "/");
        assert_eq!(
            RemoteId::from_path(&inside),
            Some(id),
            "entering an archive does not leave the connection it lives on"
        );

        // Two deep, which is the case the session cache exists for.
        let nested = inside.with_segment(BackendKind::Archive, "/inner.zip");
        assert_eq!(RemoteId::from_path(&nested), Some(id));

        // And a purely local archive is still not on any connection.
        let local = VfsPath::local("/tmp/a.zip").with_segment(BackendKind::Archive, "/");
        assert_eq!(RemoteId::from_path(&local), None);
    }
}
