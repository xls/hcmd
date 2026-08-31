//! WebDAV, over the HTTP client this program already links.
//!
//! A [`RemoteTransport`] and nothing more, which is what makes it small: the
//! quick-connect line, `hosts.toml`, the keyring, the panel, the job engine
//! and the archive machinery all work against that trait already, so a
//! WebDAV panel is a panel like any other on the day this compiles.
//!
//! `ureq` with `rustls` was linked for the update check, so this costs no new
//! dependency at all. The XML is parsed here rather than with a crate: a
//! `PROPFIND` multistatus is a handful of elements and the parser needs to be
//! forgiving about namespaces, which is easier to do directly than to
//! configure.
//!
//! # What a WebDAV server is allowed to do to you
//!
//! Everything here treats the response as hostile input. A `multistatus` names
//! paths, and a server that answers with `../../` or an absolute URL onto
//! another host is either broken or trying something; both are refused rather
//! than followed. Sizes are claims. A listing is bounded by [`MAX_ENTRIES`]
//! because the reply is a stream this program allocates from.
//!
//! # What is not here
//!
//! Locking. `LOCK` and `UNLOCK` exist and a file manager that took locks would
//! have to hold and refresh them for as long as a panel was open, which is a
//! background obligation this program does not otherwise have. Without it two
//! writers can overwrite each other, which is the same thing that happens over
//! FTP and on a network share.

use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::error::{Error, Result};
use crate::remote::transport::RemoteTransport;
use crate::remote::{Protocol, Target};
use crate::vfs::{Capabilities, Entry, LatencyClass, ReadSeek};

mod parse;

#[cfg(test)]
#[path = "dav_tests.rs"]
mod tests;

/// The most entries one `PROPFIND` may name.
///
/// The reply is parsed into a `Vec` this program allocates, and its length is
/// the server's to choose. Every other backend here has the same bound for the
/// same reason.
pub const MAX_ENTRIES: usize = 100_000;

/// The most bytes a `PROPFIND` reply may be.
pub const MAX_REPLY: u64 = 64 * 1024 * 1024;

/// One WebDAV connection.
#[derive(Debug)]
pub struct DavFs {
    /// `https://host:port`, with no trailing slash.
    origin: String,
    /// The path the connection is rooted at, `/` or `/remote.php/dav/files/me`.
    root: String,
    /// `Authorization: Basic ...`, when there are credentials.
    auth: Option<String>,
    /// Cleared by [`RemoteTransport::close`].
    live: AtomicBool,
    /// Which of the two schemes this is, for the status line.
    protocol: Protocol,
}

impl DavFs {
    /// Open a connection.
    ///
    /// Does one `PROPFIND` on the root, because a connection that cannot list
    /// its own starting directory is not connected, and finding that out here
    /// means the panel never opens onto an error.
    pub fn connect(
        target: &Target,
        user: &str,
        password: Option<&crate::remote::secret::Secret>,
    ) -> Result<Self> {
        let scheme = if target.protocol == Protocol::Davs {
            "https"
        } else {
            "http"
        };
        let port = target.port;
        let default = target.protocol.default_port();
        let origin = if port == default {
            format!("{scheme}://{}", target.host)
        } else {
            format!("{scheme}://{}:{port}", target.host)
        };
        // Exposed once, here, where the Basic header is built. The one place
        // WebDAV borrows the secret, counted by the S5 budget.
        let bytes = password
            .map(crate::remote::secret::Secret::expose)
            .unwrap_or_default();
        let auth = (!user.is_empty()).then(|| basic_auth(user, bytes));
        let fs = Self {
            origin,
            root: normalise_dir(target.dir.as_deref().unwrap_or("/")),
            auth,
            live: AtomicBool::new(true),
            protocol: target.protocol,
        };
        // Proves the credentials and the path in one request.
        fs.list(&fs.root.clone())?;
        Ok(fs)
    }

    /// Where the panel opens.
    #[must_use]
    pub fn start_dir(&self) -> &str {
        &self.root
    }

    /// The absolute URL for a path on this connection.
    fn url(&self, path: &str) -> String {
        format!("{}{}", self.origin, encode_path(path))
    }

    /// Send one request, with the credentials attached.
    ///
    /// Built through `http::Request` rather than the agent's `get`/`put`
    /// helpers, because WebDAV's verbs are `PROPFIND`, `MKCOL` and `MOVE` and
    /// those helpers only cover the ones HTTP started with.
    fn request(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<&str>,
    ) -> Result<ureq::http::Response<ureq::Body>> {
        let url = self.url(path);
        let mut builder = ureq::http::Request::builder().method(method).uri(&url);
        if let Some(auth) = self.auth.as_deref() {
            builder = builder.header("Authorization", auth);
        }
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let text = body.unwrap_or_default().to_string();
        if body.is_some() {
            builder = builder.header("Content-Type", "application/xml; charset=utf-8");
        }
        let request = builder
            .body(text)
            .map_err(|err| Error::msg(format!("{path}: {method}: {err}")))?;
        agent()
            .run(request)
            .map_err(|err| translate(method, path, &err))
    }
}

/// The HTTP agent, configured as the update check's is.
fn agent() -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(30)))
        .build();
    ureq::Agent::new_with_config(config)
}

/// `Basic` credentials, base64 of `user:password`.
fn basic_auth(user: &str, password: &[u8]) -> String {
    let mut raw = Vec::with_capacity(user.len() + 1 + password.len());
    raw.extend_from_slice(user.as_bytes());
    raw.push(b':');
    raw.extend_from_slice(password);
    format!("Basic {}", base64(&raw))
}

/// Base64, written here rather than pulled in.
///
/// One line of a header, and the alternative was a dependency for it.
fn base64(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in input.chunks(3) {
        let (a, b, c) = (
            chunk.first().copied().unwrap_or(0),
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        );
        let n = (u32::from(a) << 16) | (u32::from(b) << 8) | u32::from(c);
        for i in 0..4 {
            if i * 6 > (chunk.len() * 8).saturating_sub(1) {
                out.push('=');
            } else {
                let index = usize::try_from((n >> (18 - i * 6)) & 0x3F).unwrap_or(0);
                out.push(char::from(ALPHABET.get(index).copied().unwrap_or(b'A')));
            }
        }
    }
    out
}

/// Percent-encode a path for a URL, leaving the separators alone.
fn encode_path(path: &str) -> String {
    let mut out = String::new();
    for byte in path.bytes() {
        match byte {
            b'/' | b'-' | b'_' | b'.' | b'~' => out.push(char::from(byte)),
            b if b.is_ascii_alphanumeric() => out.push(char::from(b)),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// A directory path with exactly one trailing slash.
fn normalise_dir(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return "/".to_string();
    }
    format!("{trimmed}/")
}

/// An HTTP failure, as a sentence about what was being done.
fn translate(method: &str, path: &str, err: &ureq::Error) -> Error {
    let text = err.to_string();
    // The two a user can act on, named rather than left as a number.
    if text.contains("401") || text.contains("403") {
        return Error::msg(format!(
            "{path}: refused ({method}); check the user and password"
        ));
    }
    if text.contains("404") {
        return Error::msg(format!("{path}: not found"));
    }
    Error::msg(format!("{path}: {method} failed: {text}"))
}

/// The body sent with every `PROPFIND`.
///
/// Names the four properties a panel row needs. Asking for `allprop` instead
/// makes some servers return every dead property anybody ever set on the file,
/// which on a large directory is megabytes of XML for four fields.
const PROPFIND_BODY: &str = concat!(
    r#"<?xml version="1.0" encoding="utf-8"?>"#,
    r#"<D:propfind xmlns:D="DAV:"><D:prop>"#,
    "<D:resourcetype/><D:getcontentlength/>",
    "<D:getlastmodified/><D:displayname/>",
    "</D:prop></D:propfind>",
);

impl DavFs {
    /// The body of a reply, bounded.
    fn body_of(response: ureq::http::Response<ureq::Body>) -> Result<String> {
        let mut text = String::new();
        response
            .into_body()
            .into_reader()
            .take(MAX_REPLY)
            .read_to_string(&mut text)
            .map_err(Error::Bare)?;
        Ok(text)
    }
}

impl RemoteTransport for DavFs {
    fn protocol(&self) -> Protocol {
        self.protocol
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            writable: true,
            // No ranged reads here: `open_seek` would need `Range` requests and
            // a reader that reissues them, and the viewer's forward-only mode
            // already covers a stream. Said honestly so the viewer picks it.
            seekable: false,
            random_access: false,
            has_directories: true,
            // `MOVE` is atomic at the server, which is what this asks.
            atomic_rename: true,
            paged_listing: false,
            can_execute: false,
            links: false,
            settable_mode: false,
            latency: LatencyClass::Network,
        }
    }

    fn list(&self, dir: &str) -> Result<Vec<Entry>> {
        let dir = normalise_dir(dir);
        let response = self.request("PROPFIND", &dir, &[("Depth", "1")], Some(PROPFIND_BODY))?;
        let xml = Self::body_of(response)?;
        let rows = parse::multistatus(&xml, MAX_ENTRIES);
        Ok(rows
            .iter()
            .filter_map(|row| parse::entry_of(row, &dir, &self.origin))
            .collect())
    }

    fn stat(&self, path: &str) -> Result<Entry> {
        let response = self.request("PROPFIND", path, &[("Depth", "0")], Some(PROPFIND_BODY))?;
        let xml = Self::body_of(response)?;
        let rows = parse::multistatus(&xml, 1);
        let row = rows
            .first()
            .ok_or_else(|| Error::msg(format!("{path}: the server described nothing")))?;
        let name = path.rsplit('/').next().unwrap_or(path);
        let mut entry = if row.is_dir {
            Entry::dir(name)
        } else {
            Entry::file(name)
        };
        entry.size = row.len;
        Ok(entry)
    }

    fn read_link(&self, path: &str) -> Result<String> {
        // WebDAV has no symbolic links: a resource is a collection or it is
        // not. Said rather than answered with the path itself.
        Err(Error::Unsupported(Box::leak(
            format!("{path}: WebDAV has no symbolic links").into_boxed_str(),
        )))
    }

    fn open_read(&self, path: &str) -> Result<Box<dyn Read + Send>> {
        let response = self.request("GET", path, &[], None)?;
        Ok(Box::new(response.into_body().into_reader()))
    }

    fn open_seek(&self, path: &str) -> Result<Box<dyn ReadSeek + Send>> {
        Err(Error::Unsupported(Box::leak(
            format!("{path}: a WebDAV read is a stream, not a seekable file").into_boxed_str(),
        )))
    }

    fn open_write(&self, path: &str) -> Result<Box<dyn std::io::Write + Send>> {
        Ok(Box::new(Upload {
            fs: self.clone_handle(),
            path: path.to_string(),
            buffer: Vec::new(),
        }))
    }

    fn create_dir(&self, path: &str) -> Result<()> {
        self.request("MKCOL", path, &[], None).map(|_| ())
    }

    fn remove_file(&self, path: &str) -> Result<()> {
        self.request("DELETE", path, &[], None).map(|_| ())
    }

    fn remove_dir(&self, path: &str) -> Result<()> {
        // The same verb: DELETE on a collection removes it and everything in
        // it, which is what the copy engine has already confirmed with the
        // user by the time this is called.
        self.request(
            "DELETE",
            &normalise_dir(path),
            &[("Depth", "infinity")],
            None,
        )
        .map(|_| ())
    }

    fn rename(&self, from: &str, to: &str) -> Result<()> {
        let destination = self.url(to);
        // `Overwrite: T`, because a server is otherwise entitled to refuse
        // rather than replace, and by the time this runs the copy engine has
        // already asked the user about the collision.
        self.request(
            "MOVE",
            from,
            &[("Destination", destination.as_str()), ("Overwrite", "T")],
            None,
        )
        .map(|_| ())
    }

    fn is_live(&self) -> bool {
        self.live.load(Ordering::Relaxed)
    }

    fn close(&self) {
        self.live.store(false, Ordering::Relaxed);
    }
}

impl DavFs {
    /// A second handle onto the same connection, for an upload to hold.
    ///
    /// There is no session to share: every request carries its own
    /// credentials, so a "connection" here is the origin, the root and the
    /// header. Copying those is the whole of it.
    fn clone_handle(&self) -> Self {
        Self {
            origin: self.origin.clone(),
            root: self.root.clone(),
            auth: self.auth.clone(),
            live: AtomicBool::new(self.live.load(Ordering::Relaxed)),
            protocol: self.protocol,
        }
    }
}

/// A file being written to the server.
///
/// WebDAV has no streaming upload without either chunked encoding the server
/// may refuse or a `Content-Length` known in advance, and the copy engine
/// hands bytes over as it reads them. So the body is gathered and sent on
/// `flush`, which is where the engine ends a file.
///
/// **This is the one place here that holds a whole file in memory.** It is
/// bounded by [`MAX_UPLOAD`]: past that the write is refused rather than the
/// process growing to the size of whatever somebody dragged onto a panel.
struct Upload {
    fs: DavFs,
    path: String,
    buffer: Vec<u8>,
}

/// The most a single `PUT` may carry.
pub const MAX_UPLOAD: usize = 512 * 1024 * 1024;

impl std::io::Write for Upload {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.buffer.len().saturating_add(buf.len()) > MAX_UPLOAD {
            return Err(std::io::Error::other(format!(
                "{}: larger than the {} MB a WebDAV upload holds",
                self.path,
                MAX_UPLOAD / (1024 * 1024)
            )));
        }
        self.buffer.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if self.buffer.is_empty() {
            // An empty file is still a file, and PUT with no body creates it.
            // Returning early here would leave `touch` on a remote doing
            // nothing at all.
        }
        let url = self.fs.url(&self.path);
        let mut builder = ureq::http::Request::builder().method("PUT").uri(&url);
        if let Some(auth) = self.fs.auth.as_deref() {
            builder = builder.header("Authorization", auth);
        }
        let request = builder
            .body(self.buffer.clone())
            .map_err(|err| std::io::Error::other(err.to_string()))?;
        agent()
            .run(request)
            .map(|_| ())
            .map_err(|err| std::io::Error::other(err.to_string()))?;
        self.buffer.clear();
        Ok(())
    }
}

impl Drop for Upload {
    fn drop(&mut self) {
        // The copy engine calls `flush`; this is for every other path out,
        // including an error unwinding past it. A failure here has nowhere to
        // go, which is why `flush` is the one that reports.
        if !self.buffer.is_empty() {
            let _ = std::io::Write::flush(self);
        }
    }
}
