//! Sending files to one device: prepare, upload each file, cancel.
//!
//! One connection per request, closed after - the protocol is stateless past
//! the session id, the requests are few, and a fresh handshake per file is
//! nothing next to the file. Every request pins the device's fingerprint
//! through [`super::tls`], so a device that changed identity mid-transfer is
//! refused rather than fed the rest.
//!
//! The status codes are the specification's: `204` there was nothing to
//! send, `401` a PIN is wanted, `403` the other side said no, `409` it is
//! busy with somebody else, `429` slow down. Each is its own
//! [`SendError`] so the caller can ask for a PIN or say "declined" rather
//! than print a number.

use std::collections::BTreeMap;
use std::fmt;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use super::http::{self, Body};
use super::protocol::{
    DeviceInfo, DeviceType, FileMeta, PrepareUploadRequest, PrepareUploadResponse, Protocol,
};
use super::tls::{self, Connection};
use super::{API, PORT};

/// A device to send to: where, how, and who it said it was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    /// The name it gave itself.
    pub alias: String,
    /// The address to connect to: an IP literal, or a name that was typed.
    pub host: String,
    /// The port.
    pub port: u16,
    /// In the clear or over TLS.
    pub protocol: Protocol,
    /// The fingerprint it announced, pinned on every connection. `None` for
    /// a device reached by a typed address, which announced nothing.
    pub fingerprint: Option<String>,
    /// What kind of device, for the list.
    pub device_type: Option<DeviceType>,
    /// Its model or OS, for the list.
    pub model: Option<String>,
}

impl Peer {
    /// A device from what it announced and where the announcement came from.
    #[must_use]
    pub fn from_announcement(device: &DeviceInfo, from: IpAddr) -> Self {
        Self {
            alias: device.alias.clone(),
            host: from.to_string(),
            port: device.port.unwrap_or(PORT),
            protocol: device.protocol.unwrap_or_default(),
            fingerprint: (device.protocol.unwrap_or_default() == Protocol::Https)
                .then(|| device.fingerprint.clone()),
            device_type: device.device_type,
            model: device.device_model.clone(),
        }
    }

    /// A device at a typed address: `host` or `host:port`, HTTPS assumed.
    #[must_use]
    pub fn typed(address: &str) -> Self {
        let address = address.trim();
        let (host, port) = match address.rsplit_once(':') {
            // `[::1]:53317` and `10.0.0.5:53317`, but not a bare IPv6 literal.
            Some((h, p)) if !h.contains(':') || h.ends_with(']') => {
                (h.trim_matches(['[', ']']), p.parse().unwrap_or(PORT))
            }
            _ => (address.trim_matches(['[', ']']), PORT),
        };
        Self {
            alias: address.to_string(),
            host: host.to_string(),
            port,
            protocol: Protocol::Https,
            fingerprint: None,
            device_type: None,
            model: None,
        }
    }

    /// The `Host` header.
    fn host_header(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

/// Why a send did not happen, in the protocol's own terms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendError {
    /// `403`: the other side said no.
    Declined,
    /// `401`: the other side wants a PIN, or the one given was wrong.
    PinRequired,
    /// `409`: the other side is in a session with somebody else.
    Busy,
    /// `429`: asked too often.
    TooManyRequests,
    /// `204`: the other side found nothing it needed.
    NothingToSend,
    /// The progress callback said stop.
    Cancelled,
    /// Any other status, with what the body said.
    Refused(u16, String),
    /// The connection, the TLS pin, or the answer could not be read.
    Io(String),
}

impl fmt::Display for SendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Declined => f.write_str("the device declined"),
            Self::PinRequired => f.write_str("the device wants a PIN"),
            Self::Busy => f.write_str("the device is busy with another transfer"),
            Self::TooManyRequests => f.write_str("the device asked to slow down"),
            Self::NothingToSend => f.write_str("the device needed none of the files"),
            Self::Cancelled => f.write_str("cancelled"),
            Self::Refused(status, body) => {
                let body = body.trim();
                if body.is_empty() {
                    write!(f, "the device answered {status}")
                } else {
                    write!(f, "the device answered {status}: {body}")
                }
            }
            Self::Io(why) => f.write_str(why),
        }
    }
}

impl From<std::io::Error> for SendError {
    fn from(err: std::io::Error) -> Self {
        if err.kind() == std::io::ErrorKind::Interrupted {
            Self::Cancelled
        } else {
            Self::Io(err.to_string())
        }
    }
}

/// An accepted `prepare-upload`: the session and a token per file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    /// The session id.
    pub id: String,
    /// Token by file id. A file missing here was declined by the receiver.
    pub tokens: BTreeMap<String, String>,
}

/// This side, sending.
#[derive(Debug, Clone)]
pub struct Sender {
    /// Who we say we are in every `prepare-upload`.
    me: DeviceInfo,
}

impl Sender {
    /// A sender introducing itself as `me`.
    #[must_use]
    pub fn new(me: DeviceInfo) -> Self {
        Self { me }
    }

    /// Open a connection to `peer`, pinned when it announced a fingerprint.
    fn connect(peer: &Peer) -> Result<Connection, SendError> {
        let connection = match peer.protocol {
            Protocol::Http => tls::connect_plain(&peer.host, peer.port),
            Protocol::Https => tls::connect_tls(&peer.host, peer.port, peer.fingerprint.as_deref()),
        };
        connection.map_err(|e| SendError::Io(format!("{}: {e}", peer.alias)))
    }

    /// `GET /info`: what the device at `peer` says about itself, and the
    /// fingerprint of the certificate it presented, for a typed address.
    pub fn info(peer: &Peer) -> Result<(DeviceInfo, Option<String>), SendError> {
        let mut connection = Self::connect(peer)?;
        let response = http::request(
            connection.stream.as_mut(),
            &peer.host_header(),
            "GET",
            &format!("{API}/info"),
            Body::None,
            &mut |_| true,
        )?;
        if response.status != 200 {
            return Err(SendError::Refused(response.status, response.text()));
        }
        let device: DeviceInfo = serde_json::from_slice(&response.body)
            .map_err(|e| SendError::Io(format!("{}: info was not readable: {e}", peer.alias)))?;
        Ok((device, connection.fingerprint))
    }

    /// `POST /prepare-upload`: offer `files`; the answer is the session to
    /// upload them in. Retried by the caller with a `pin` after
    /// [`SendError::PinRequired`].
    pub fn prepare(
        &self,
        peer: &Peer,
        files: &[FileMeta],
        pin: Option<&str>,
    ) -> Result<Session, SendError> {
        let request = PrepareUploadRequest {
            info: self.me.clone(),
            files: files.iter().map(|f| (f.id.clone(), f.clone())).collect(),
        };
        let body = serde_json::to_vec(&request)
            .map_err(|e| SendError::Io(format!("could not write the request: {e}")))?;
        let target = match pin {
            Some(pin) => format!(
                "{API}/prepare-upload?pin={}",
                crate::serve::http::percent_encode(pin)
            ),
            None => format!("{API}/prepare-upload"),
        };
        let mut connection = Self::connect(peer)?;
        let response = http::request(
            connection.stream.as_mut(),
            &peer.host_header(),
            "POST",
            &target,
            Body::Bytes(&body, "application/json"),
            &mut |_| true,
        )?;
        match response.status {
            200 => {
                let answer: PrepareUploadResponse = serde_json::from_slice(&response.body)
                    .map_err(|e| {
                        SendError::Io(format!("{}: the answer was not readable: {e}", peer.alias))
                    })?;
                Ok(Session {
                    id: answer.session_id,
                    tokens: answer.files,
                })
            }
            other => Err(status_error(other, &response.text())),
        }
    }

    /// `POST /upload`: send the file at `path` described by `file`, calling
    /// `progress` with each chunk's bytes; `false` cancels.
    pub fn upload(
        peer: &Peer,
        session: &Session,
        file: &FileMeta,
        path: &Path,
        progress: &mut dyn FnMut(u64) -> bool,
    ) -> Result<(), SendError> {
        let Some(token) = session.tokens.get(&file.id) else {
            return Err(SendError::Declined);
        };
        let mut reader = std::fs::File::open(path)
            .map_err(|e| SendError::Io(format!("{}: {e}", path.display())))?;
        let target = format!(
            "{API}/upload?sessionId={}&fileId={}&token={}",
            crate::serve::http::percent_encode(&session.id),
            crate::serve::http::percent_encode(&file.id),
            crate::serve::http::percent_encode(token)
        );
        let mut connection = Self::connect(peer)?;
        let response = http::request(
            connection.stream.as_mut(),
            &peer.host_header(),
            "POST",
            &target,
            Body::Stream(&mut reader, file.size),
            progress,
        )?;
        match response.status {
            200..=299 => Ok(()),
            other => Err(status_error(other, &response.text())),
        }
    }

    /// `POST /cancel`: tell the device the session is over. Best effort:
    /// the session is over whether or not it hears.
    pub fn cancel(peer: &Peer, session: &Session) -> Result<(), SendError> {
        let mut connection = Self::connect(peer)?;
        let target = format!(
            "{API}/cancel?sessionId={}",
            crate::serve::http::percent_encode(&session.id)
        );
        let response = http::request(
            connection.stream.as_mut(),
            &peer.host_header(),
            "POST",
            &target,
            Body::None,
            &mut |_| true,
        )?;
        match response.status {
            200..=299 => Ok(()),
            other => Err(status_error(other, &response.text())),
        }
    }
}

/// A non-200 status as the error it means.
fn status_error(status: u16, body: &str) -> SendError {
    match status {
        204 => SendError::NothingToSend,
        401 => SendError::PinRequired,
        403 => SendError::Declined,
        409 => SendError::Busy,
        429 => SendError::TooManyRequests,
        other => SendError::Refused(other, body.to_string()),
    }
}

/// The files behind `paths`, described for a `prepare-upload`: a file under
/// its own name, a folder walked with each file named by its path inside
/// the folder (`folder/sub/file`), which is how the stock app sends a folder.
/// Symlinks are followed for files and not for folders, so a loop cannot
/// run this forever.
pub fn collect(paths: &[PathBuf]) -> std::io::Result<Vec<(FileMeta, PathBuf)>> {
    let mut out = Vec::new();
    for path in paths {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "file".to_string());
        if path.is_dir() {
            walk(path, &name, &mut out)?;
        } else {
            out.push((FileMeta::of(path, &name)?, path.clone()));
        }
    }
    Ok(out)
}

/// Walk `dir`, naming each file `prefix/…`.
fn walk(dir: &Path, prefix: &str, out: &mut Vec<(FileMeta, PathBuf)>) -> std::io::Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)?.filter_map(Result::ok).collect();
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let name = format!("{prefix}/{}", entry.file_name().to_string_lossy());
        let kind = entry.file_type()?;
        if kind.is_dir() {
            walk(&path, &name, out)?;
        } else if kind.is_file() || (kind.is_symlink() && path.is_file()) {
            out.push((FileMeta::of(&path, &name)?, path));
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "send_tests.rs"]
mod tests;
