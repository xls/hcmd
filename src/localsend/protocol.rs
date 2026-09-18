//! The LocalSend wire format, and nothing else.
//!
//! Everything a device says about itself and every request of the send
//! side, as the protocol specification writes them - camel-cased JSON with a
//! handful of optional fields. Kept free of sockets so the round trip can be
//! tested against the specification's own examples.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

/// The protocol version this side speaks. Devices on 2.1 and 2.2 accept it;
/// the additions since are optional fields this side does not need.
pub const PROTOCOL_VERSION: &str = "2.0";

/// `http` or `https`: how a device wants to be spoken to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    /// Plain HTTP; the fingerprint is a random string.
    Http,
    /// HTTPS with a self-signed certificate whose SHA-256 is the fingerprint.
    #[default]
    Https,
}

/// What kind of device, for the icon the other side draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum DeviceType {
    /// A phone or tablet.
    Mobile,
    /// A desktop or laptop.
    Desktop,
    /// A browser.
    Web,
    /// No screen of its own: a terminal, a server.
    #[default]
    Headless,
    /// A server.
    Server,
    /// A kind this version does not know; kept rather than refused.
    #[serde(other)]
    Unknown,
}

/// A device as it describes itself, in an announcement, a `register`, an
/// `info` answer and the `info` of a `prepare-upload`.
///
/// `port` and `protocol` are absent from an `info` or `register` *answer* -
/// the asker already knows where it asked - so they are optional here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceInfo {
    /// The name the user gave the device.
    pub alias: String,
    /// The protocol version the device speaks.
    pub version: String,
    /// The model, or the operating system: "Samsung", "Windows".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_model: Option<String>,
    /// What kind of device.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_type: Option<DeviceType>,
    /// The identity: SHA-256 of the certificate over HTTPS, random over HTTP.
    pub fingerprint: String,
    /// The port it listens on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// How it wants to be spoken to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<Protocol>,
    /// Whether its download API is on. This side never uses it.
    #[serde(default)]
    pub download: bool,
}

impl DeviceInfo {
    /// This copy of hcmd as a device: `alias`, listening on `port` over
    /// `protocol`, with a fresh random fingerprint.
    #[must_use]
    pub fn ours(alias: &str, port: u16, protocol: Protocol) -> Self {
        Self {
            alias: alias.to_string(),
            version: PROTOCOL_VERSION.to_string(),
            device_model: Some(model()),
            device_type: Some(DeviceType::Headless),
            fingerprint: random_id("fingerprint"),
            port: Some(port),
            protocol: Some(protocol),
            download: false,
        }
    }

    /// The version as major and minor, or `None` when it does not parse.
    #[must_use]
    pub fn version_parts(&self) -> Option<(u32, u32)> {
        let (major, minor) = self.version.split_once('.')?;
        Some((major.parse().ok()?, minor.parse().ok()?))
    }
}

/// The operating system, as the other side's list shows it.
fn model() -> String {
    let os = std::env::consts::OS;
    let mut chars = os.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => "Unknown".to_string(),
    }
}

/// A multicast announcement: a device, plus whether it wants answers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Announcement {
    /// The device.
    #[serde(flatten)]
    pub device: DeviceInfo,
    /// True when the device is asking everybody to introduce themselves;
    /// false when this is such an introduction.
    #[serde(default)]
    pub announce: bool,
}

/// The times a file carries with it, RFC 3339.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct FileTimes {
    /// Last modified.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified: Option<String>,
    /// Last accessed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accessed: Option<String>,
}

/// One file in a `prepare-upload`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileMeta {
    /// An id unique within the request; the token comes back keyed on it.
    pub id: String,
    /// The name, with a relative path in it when a folder is sent.
    pub file_name: String,
    /// Size in bytes.
    pub size: u64,
    /// The MIME type.
    pub file_type: String,
    /// SHA-256 of the content, when the sender bothered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// A preview (a thumbnail), when the sender bothered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    /// The file's times.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<FileTimes>,
}

impl FileMeta {
    /// Describe the local file at `path`, sent under `name`. The size and the
    /// modified time come from the file; the type from its extension.
    pub fn of(path: &Path, name: &str) -> std::io::Result<Self> {
        let meta = std::fs::metadata(path)?;
        let modified = meta.modified().ok().map(rfc3339);
        Ok(Self {
            id: random_id(name),
            file_name: name.to_string(),
            size: meta.len(),
            file_type: mime_guess::from_path(path)
                .first_or_octet_stream()
                .essence_str()
                .to_string(),
            sha256: None,
            preview: None,
            metadata: Some(FileTimes {
                modified,
                accessed: None,
            }),
        })
    }
}

/// `2021-01-01T12:34:56Z`, as the specification's example writes it.
fn rfc3339(at: SystemTime) -> String {
    let stamp: chrono::DateTime<chrono::Utc> = at.into();
    stamp.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// The body of a `prepare-upload`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrepareUploadRequest {
    /// Who is sending.
    pub info: DeviceInfo,
    /// What, keyed by each file's id.
    pub files: BTreeMap<String, FileMeta>,
}

/// The answer to a `prepare-upload` the receiver accepted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrepareUploadResponse {
    /// The session every upload and the cancel name.
    pub session_id: String,
    /// A token per accepted file, keyed by the file's id. A file the receiver
    /// left out was declined.
    pub files: BTreeMap<String, String>,
}

/// A random-looking id: 32 hex characters of a SHA-256 over the clock, the
/// process, a counter and `salt`. Not a secret - the fingerprint over HTTP
/// and the file ids are identifiers, not keys - so the clock is entropy
/// enough, and no random-number crate is needed.
#[must_use]
pub fn random_id(salt: &str) -> String {
    use sha2::{Digest, Sha256};
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let mut hasher = Sha256::new();
    hasher.update(nanos.to_le_bytes());
    hasher.update(std::process::id().to_le_bytes());
    hasher.update(COUNTER.fetch_add(1, Ordering::Relaxed).to_le_bytes());
    hasher.update(salt.as_bytes());
    let digest = hasher.finalize();
    digest.iter().take(16).map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
#[path = "protocol_tests.rs"]
mod tests;
