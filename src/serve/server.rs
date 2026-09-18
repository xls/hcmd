//! The listener, and the thread per connection behind it.
//!
//! Plain `std::net`: a listener polled with a short sleep so it can notice
//! the stop flag, and one thread per connection that reads a head, answers
//! it and closes. No pool, no keep-alive: a share on a LAN sees a handful of
//! connections, and a thread that lives for one request is the simplest thing
//! that cannot leak one. What each request did goes back to the dialog as a
//! [`ServeEvent`] over a channel that is never blocked on - a slow screen
//! drops a log line rather than stalling a transfer.

use std::io::{Read, Seek, SeekFrom, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::mpsc;

use super::dav::{self, DavResource};
use super::http::{
    self, Body, IndexEntry, MAX_HEAD, ParseError, Request, Response, content_type, http_date,
    index_html, percent_encode,
};
use super::tree::{self, Resolved, Root};

/// How deep the [`ServeEvent`] channel is: a burst of requests past this is
/// dropped from the log, never waited on.
pub const SERVE_CHANNEL_DEPTH: usize = 64;

/// What one request did, for the dialog's log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Served {
    /// Who asked.
    pub peer: String,
    /// `GET`, `PROPFIND`, ...
    pub method: String,
    /// The path as requested, decoded.
    pub path: String,
    /// What they got.
    pub status: u16,
    /// How many body bytes went out.
    pub bytes: u64,
}

/// What the server tells the dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServeEvent {
    /// A request was answered.
    Request(Served),
    /// The listener stopped on its own, with why.
    Failed(String),
}

/// A running share. Dropping it stops the listener.
#[derive(Debug)]
pub struct Server {
    stop: Arc<AtomicBool>,
    addr: SocketAddr,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    /// Serve `roots` on every interface, on a free port.
    pub fn start(roots: Vec<Root>, tx: mpsc::Sender<ServeEvent>) -> std::io::Result<Self> {
        Self::start_on("0.0.0.0:0", roots, tx)
    }

    /// Serve `roots` on `bind` (`host:port`, port `0` for any free one).
    pub fn start_on(
        bind: &str,
        roots: Vec<Root>,
        tx: mpsc::Sender<ServeEvent>,
    ) -> std::io::Result<Self> {
        let listener = TcpListener::bind(bind)?;
        listener.set_nonblocking(true)?;
        let addr = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let roots = Arc::new(roots);
        let thread = std::thread::Builder::new()
            .name("hcmd-serve".to_string())
            .spawn(move || accept_loop(&listener, &flag, &roots, &tx))?;
        Ok(Self {
            stop,
            addr,
            thread: Some(thread),
        })
    }

    /// The port it listens on.
    #[must_use]
    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    /// Stop listening. Connections already being answered finish.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The address this machine reaches the network with - the one to print, so
/// somebody on the LAN has a URL that works rather than `0.0.0.0`.
///
/// A UDP socket "connected" to a public address: no packet is sent, but the
/// kernel picks the interface it would use, and its address is the answer.
/// `None` on a machine with no route out, where `localhost` is all there is.
#[must_use]
pub fn lan_ip() -> Option<IpAddr> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("10.254.254.254:1").ok()?;
    let ip = socket.local_addr().ok()?.ip();
    (!ip.is_unspecified() && !ip.is_loopback()).then_some(ip)
}

/// Accept until told to stop, handing each connection its own thread.
fn accept_loop(
    listener: &TcpListener,
    stop: &AtomicBool,
    roots: &Arc<Vec<Root>>,
    tx: &mpsc::Sender<ServeEvent>,
) {
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, peer)) => {
                let roots = Arc::clone(roots);
                let tx = tx.clone();
                let _ = std::thread::Builder::new()
                    .name("hcmd-serve-conn".to_string())
                    .spawn(move || {
                        if let Some(served) = handle(stream, peer, &roots) {
                            let _ = tx.try_send(ServeEvent::Request(served));
                        }
                    });
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                let _ = tx.try_send(ServeEvent::Failed(e.to_string()));
                return;
            }
        }
    }
}

/// Answer one connection: read the head, respond, close.
fn handle(mut stream: TcpStream, peer: SocketAddr, roots: &[Root]) -> Option<Served> {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));

    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0_u8; 1024];
    let request = loop {
        match http::parse_request(&buf) {
            Ok(request) => break Ok(request),
            Err(ParseError::Incomplete) if buf.len() <= MAX_HEAD => {}
            Err(other) => break Err(other),
        }
        match stream.read(&mut chunk) {
            Ok(0) => break Err(ParseError::Malformed),
            Ok(n) => buf.extend_from_slice(chunk.get(..n).unwrap_or(&[])),
            Err(_) => break Err(ParseError::Malformed),
        }
    };

    let (response, method, path) = match request {
        Ok(request) => {
            let response = respond(roots, &request);
            (response, request.method, request.path)
        }
        Err(ParseError::TooLarge) => (
            Response::text(431, "request head too large"),
            String::from("?"),
            String::new(),
        ),
        Err(_) => (
            Response::text(400, "bad request"),
            String::from("?"),
            String::new(),
        ),
    };
    let bytes = write_response(&mut stream, &response);
    let _ = stream.shutdown(std::net::Shutdown::Both);
    Some(Served {
        peer: peer.ip().to_string(),
        method,
        path,
        status: response.status,
        bytes,
    })
}

/// Write the head and stream the body. Returns the body bytes that went out.
fn write_response(stream: &mut TcpStream, response: &Response) -> u64 {
    if stream.write_all(&response.head()).is_err() {
        return 0;
    }
    match &response.body {
        Body::Empty => 0,
        Body::Bytes(bytes) => {
            if stream.write_all(bytes).is_ok() {
                bytes.len() as u64
            } else {
                0
            }
        }
        Body::File { path, start, len } => stream_file(stream, path, *start, *len),
    }
}

/// Copy `len` bytes of `path` from `start` onto the stream, a chunk at a time.
fn stream_file(stream: &mut TcpStream, path: &Path, start: u64, len: u64) -> u64 {
    let Ok(mut file) = std::fs::File::open(path) else {
        return 0;
    };
    if file.seek(SeekFrom::Start(start)).is_err() {
        return 0;
    }
    let mut left = len;
    let mut sent = 0_u64;
    let mut buf = vec![0_u8; 64 * 1024];
    while left > 0 {
        let want = usize::try_from(left).unwrap_or(usize::MAX).min(buf.len());
        let n = match file.read(buf.get_mut(..want).unwrap_or(&mut [])) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        if stream.write_all(buf.get(..n).unwrap_or(&[])).is_err() {
            break;
        }
        sent = sent.saturating_add(n as u64);
        left = left.saturating_sub(n as u64);
    }
    sent
}

/// The share's answer to one request.
#[must_use]
pub fn respond(roots: &[Root], request: &Request) -> Response {
    match request.method.as_str() {
        "OPTIONS" => {
            return Response::new(204)
                .header("Allow", dav::ALLOW)
                .header("DAV", "1");
        }
        "GET" | "HEAD" | "PROPFIND" => {}
        _ => {
            return Response::text(405, "this share is read-only").header("Allow", dav::ALLOW);
        }
    }
    let head_only = request.method == "HEAD";
    let response = match tree::resolve(roots, &request.path) {
        Resolved::Forbidden => Response::text(403, "not on this share"),
        Resolved::NotFound => Response::text(404, "no such file on this share"),
        Resolved::Index => index_response(roots, request),
        Resolved::Path { root, path, url } => {
            let root_path = roots
                .iter()
                .find(|r| r.name == root)
                .map(|r| r.path.as_path());
            match (root_path, std::fs::metadata(&path)) {
                (Some(root_path), Ok(meta)) if tree::stays_inside(root_path, &path) => {
                    if meta.is_dir() {
                        dir_response(&path, &url, request)
                    } else {
                        file_response(&path, &meta, &url, request)
                    }
                }
                _ => Response::text(404, "no such file on this share"),
            }
        }
    };
    if head_only {
        response.without_body()
    } else {
        response
    }
}

/// A file's entry for an index or a `PROPFIND`, or `None` for one that is
/// not there or leads out of the share.
fn entry_of(dir: &Path, name: &str) -> Option<IndexEntry> {
    let path = dir.join(name);
    let meta = std::fs::metadata(&path).ok()?;
    if !tree::stays_inside(dir, &path) {
        return None;
    }
    let is_dir = meta.is_dir();
    let mut href = percent_encode(name);
    if is_dir {
        href.push('/');
    }
    Some(IndexEntry {
        name: name.to_string(),
        href,
        is_dir,
        size: if is_dir { 0 } else { meta.len() },
        modified: meta.modified().ok(),
    })
}

/// The entries of a real directory, sorted by the index page later.
fn dir_entries(dir: &Path) -> Vec<IndexEntry> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    read.filter_map(Result::ok)
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        .filter_map(|name| entry_of(dir, &name))
        .collect()
}

/// The share's root: every selected entry, by its share name.
fn root_entries(roots: &[Root]) -> Vec<IndexEntry> {
    roots
        .iter()
        .filter_map(|root| {
            let meta = std::fs::metadata(&root.path).ok()?;
            let is_dir = meta.is_dir();
            let mut href = percent_encode(&root.name);
            if is_dir {
                href.push('/');
            }
            Some(IndexEntry {
                name: root.name.clone(),
                href,
                is_dir,
                size: if is_dir { 0 } else { meta.len() },
                modified: meta.modified().ok(),
            })
        })
        .collect()
}

/// An index entry as a `PROPFIND` resource under `base` (which ends in `/`).
fn resource_under(base: &str, entry: &IndexEntry) -> DavResource {
    DavResource {
        href: format!("{base}{}", entry.href),
        name: entry.name.clone(),
        is_dir: entry.is_dir,
        size: entry.size,
        modified: entry.modified,
        content_type: if entry.is_dir {
            String::new()
        } else {
            content_type(Path::new(&entry.name))
        },
    }
}

/// The collection itself, as a `PROPFIND` resource.
fn collection(href: &str, name: &str, modified: Option<std::time::SystemTime>) -> DavResource {
    DavResource {
        href: href.to_string(),
        name: name.to_string(),
        is_dir: true,
        size: 0,
        modified,
        content_type: String::new(),
    }
}

/// A `207` carrying `resources`.
fn multistatus_response(resources: &[DavResource]) -> Response {
    Response::new(207).bytes(
        "application/xml; charset=utf-8",
        dav::multistatus(resources).into_bytes(),
    )
}

/// The share's root, as a page or as a `PROPFIND`.
fn index_response(roots: &[Root], request: &Request) -> Response {
    let entries = root_entries(roots);
    if request.method == "PROPFIND" {
        let mut resources = vec![collection("/", "", None)];
        if dav::depth(request.header("depth")) > 0 {
            resources.extend(entries.iter().map(|e| resource_under("/", e)));
        }
        return multistatus_response(&resources);
    }
    Response::new(200).bytes(
        "text/html; charset=utf-8",
        index_html("Shared by hcmd", None, &entries).into_bytes(),
    )
}

/// A directory on the share, as a page or as a `PROPFIND`.
fn dir_response(path: &Path, url: &str, request: &Request) -> Response {
    // A folder's links are relative to it, which needs the trailing slash:
    // send a browser there rather than serve a page whose links are wrong.
    if !url.ends_with('/') {
        let there = format!("{url}/");
        return Response::new(301).header("Location", &there).bytes(
            "text/plain; charset=utf-8",
            format!("{there}\n").into_bytes(),
        );
    }
    let entries = dir_entries(path);
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_string();
    if request.method == "PROPFIND" {
        let modified = std::fs::metadata(path).ok().and_then(|m| m.modified().ok());
        let mut resources = vec![collection(url, &name, modified)];
        if dav::depth(request.header("depth")) > 0 {
            resources.extend(entries.iter().map(|e| resource_under(url, e)));
        }
        return multistatus_response(&resources);
    }
    // Under a root the parent is the share's own index; deeper, the folder
    // above.
    let parent = if url.matches('/').count() <= 2 {
        "/"
    } else {
        "../"
    };
    Response::new(200).bytes(
        "text/html; charset=utf-8",
        index_html(url, Some(parent), &entries).into_bytes(),
    )
}

/// A file on the share: whole, a range of it, or its `PROPFIND` properties.
fn file_response(path: &Path, meta: &std::fs::Metadata, url: &str, request: &Request) -> Response {
    let len = meta.len();
    let modified = meta.modified().ok();
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_string();
    let mime = content_type(path);
    if request.method == "PROPFIND" {
        return multistatus_response(&[DavResource {
            href: url.to_string(),
            name,
            is_dir: false,
            size: len,
            modified,
            content_type: mime,
        }]);
    }
    let mut response = Response::new(200).header("Content-Type", &mime);
    if let Some(at) = modified {
        response = response.header("Last-Modified", http_date(at));
    }
    match http::resolve_range(request.header("range"), len) {
        Some(Err(())) => Response::text(416, "range not satisfiable")
            .header("Content-Range", format!("bytes */{len}")),
        Some(Ok((first, last))) => {
            let count = last.saturating_sub(first).saturating_add(1);
            response.status = 206;
            response = response
                .header("Content-Range", format!("bytes {first}-{last}/{len}"))
                .header("Content-Length", count.to_string());
            response.body = Body::File {
                path: path.to_path_buf(),
                start: first,
                len: count,
            };
            response
        }
        None => {
            response = response.header("Content-Length", len.to_string());
            response.body = Body::File {
                path: path.to_path_buf(),
                start: 0,
                len,
            };
            response
        }
    }
}

#[cfg(test)]
#[path = "server_tests.rs"]
mod tests;
