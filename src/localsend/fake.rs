//! A fake LocalSend receiver on loopback, for tests up and down the crate.
//!
//! Speaks the receiving half of the protocol the way the specification says
//! a receiver does - `info`, `prepare-upload` with its status codes, `upload`
//! checked against the tokens it issued, `cancel` - in the clear, so a test
//! needs no certificate. It records what it saw so a test can assert on the
//! bytes that arrived rather than on the sender's word.

use std::collections::BTreeMap;
use std::io::Write;
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use super::protocol::{
    DeviceInfo, DeviceType, PrepareUploadRequest, PrepareUploadResponse, Protocol,
};
use super::send::Peer;
use crate::serve::http::{Body, Response, read_request};

/// How the fake answers a `prepare-upload`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Accept everything.
    Accept,
    /// Want the PIN `1234` first.
    Pin,
    /// Say no.
    Decline,
    /// Say busy.
    Busy,
}

/// What the fake saw.
#[derive(Debug, Default)]
pub struct Seen {
    /// The `prepare-upload` bodies, parsed.
    pub prepared: Vec<PrepareUploadRequest>,
    /// Uploaded bytes by file name, with the `Content-Length` they came with.
    pub uploaded: BTreeMap<String, (u64, Vec<u8>)>,
    /// Whether `cancel` came for the session.
    pub cancelled: bool,
    /// The `Host` header of the last request.
    pub host: String,
}

/// The fake, listening until dropped.
pub struct Receiver {
    /// The port it took.
    pub port: u16,
    /// What it has seen so far.
    pub seen: Arc<Mutex<Seen>>,
    stop: Arc<AtomicBool>,
}

impl Receiver {
    /// Start one on a free loopback port, answering `prepare-upload` as
    /// `mode` says.
    pub fn start(mode: Mode) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let seen = Arc::new(Mutex::new(Seen::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let (seen2, stop2) = (Arc::clone(&seen), Arc::clone(&stop));
        listener.set_nonblocking(true).expect("nonblocking");
        std::thread::spawn(move || {
            // Files by id → (name, token) for the one session this fake keeps.
            let mut files: BTreeMap<String, (String, String)> = BTreeMap::new();
            while !stop2.load(Ordering::Relaxed) {
                let Ok((mut stream, _)) = listener.accept() else {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                    continue;
                };
                stream.set_nonblocking(false).expect("blocking");
                let Ok((request, body)) = read_request(&mut stream, 16 * 1024 * 1024) else {
                    continue;
                };
                let mut seen = seen2.lock().expect("lock");
                seen.host = request.header("host").unwrap_or("").to_string();
                let response = match (request.method.as_str(), request.path.as_str()) {
                    ("GET", "/api/localsend/v2/info") => {
                        let me = DeviceInfo {
                            alias: "Fake Phone".into(),
                            version: "2.1".into(),
                            device_model: Some("Pixel".into()),
                            device_type: Some(DeviceType::Mobile),
                            fingerprint: "fake-fingerprint".into(),
                            port: None,
                            protocol: None,
                            download: false,
                        };
                        Response::new(200)
                            .bytes("application/json", serde_json::to_vec(&me).expect("json"))
                    }
                    ("POST", "/api/localsend/v2/prepare-upload") => {
                        let parsed: PrepareUploadRequest =
                            serde_json::from_slice(&body).expect("a well-formed prepare-upload");
                        seen.prepared.push(parsed.clone());
                        let pin = request.query_param("pin");
                        match mode {
                            Mode::Decline => Response::text(403, "Rejected"),
                            Mode::Busy => Response::text(409, "Blocked by another session"),
                            Mode::Pin if pin.as_deref() != Some("1234") => {
                                Response::text(401, "PIN required / Invalid PIN")
                            }
                            Mode::Pin | Mode::Accept => {
                                files.clear();
                                let mut tokens = BTreeMap::new();
                                for (id, meta) in &parsed.files {
                                    let token = format!("tok-{id}");
                                    files.insert(
                                        id.clone(),
                                        (meta.file_name.clone(), token.clone()),
                                    );
                                    tokens.insert(id.clone(), token);
                                }
                                let answer = PrepareUploadResponse {
                                    session_id: "sess-1".into(),
                                    files: tokens,
                                };
                                Response::new(200).bytes(
                                    "application/json",
                                    serde_json::to_vec(&answer).expect("json"),
                                )
                            }
                        }
                    }
                    ("POST", "/api/localsend/v2/upload") => {
                        let session = request.query_param("sessionId").unwrap_or_default();
                        let id = request.query_param("fileId").unwrap_or_default();
                        let token = request.query_param("token").unwrap_or_default();
                        match files.get(&id) {
                            Some((name, want)) if session == "sess-1" && *want == token => {
                                seen.uploaded
                                    .insert(name.clone(), (request.content_length(), body));
                                Response::new(200)
                            }
                            _ => Response::text(403, "Invalid token or IP address"),
                        }
                    }
                    ("POST", "/api/localsend/v2/cancel") => {
                        seen.cancelled =
                            request.query_param("sessionId").as_deref() == Some("sess-1");
                        Response::new(200)
                    }
                    _ => Response::text(404, "no such thing"),
                };
                let _ = stream.write_all(&response.head());
                if let Body::Bytes(bytes) = &response.body {
                    let _ = stream.write_all(bytes);
                }
                let _ = stream.flush();
            }
        });
        Self { port, seen, stop }
    }

    /// The fake as a peer to send to.
    pub fn peer(&self) -> Peer {
        Peer {
            alias: "Fake Phone".into(),
            host: "127.0.0.1".into(),
            port: self.port,
            protocol: Protocol::Http,
            fingerprint: None,
            device_type: Some(DeviceType::Mobile),
            model: Some("Pixel".into()),
        }
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// A scratch tree: a lone file and a folder with a nested file. Returns the
/// root to delete and the two paths to send.
pub fn scratch(tag: &str) -> (std::path::PathBuf, Vec<std::path::PathBuf>) {
    let dir = std::env::temp_dir().join(format!("hcmd-ls-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("album/2026")).expect("dirs");
    let big: Vec<u8> = (0..300_000_u32).map(|i| (i % 253) as u8).collect();
    std::fs::write(dir.join("album/big.bin"), &big).expect("file");
    std::fs::write(dir.join("album/2026/note.txt"), b"hello\n").expect("file");
    std::fs::write(dir.join("report.pdf"), b"%PDF-1.4\n").expect("file");
    (dir.clone(), vec![dir.join("report.pdf"), dir.join("album")])
}
