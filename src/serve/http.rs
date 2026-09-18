//! The HTTP the share speaks: one request parsed off the wire, one response
//! built for it.
//!
//! HTTP/1.1 as a static file server needs it and no more: a request line,
//! headers, `Range`, and responses with a length. No chunking, no keep-alive
//! bookkeeping beyond closing the connection, no compression. Everything here
//! is a pure function over bytes and strings, so the wire format is tested
//! without a socket.

use std::path::Path;
use std::time::SystemTime;

/// The most a request head may be, before it is refused rather than read.
pub const MAX_HEAD: usize = 16 * 1024;

/// One request, as far as the share cares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// `GET`, `HEAD`, `PROPFIND`, `OPTIONS`, ... upper-cased.
    pub method: String,
    /// The path, percent-decoded, without the query. Always starts with `/`.
    pub path: String,
    /// The query as it came, after the `?` and still percent-encoded; empty
    /// when there was none. [`Request::query_param`] reads one value.
    pub query: String,
    /// The headers, names lower-cased.
    pub headers: Vec<(String, String)>,
}

impl Request {
    /// A query parameter's value, percent-decoded, or `None` when absent.
    #[must_use]
    pub fn query_param(&self, name: &str) -> Option<String> {
        self.query.split('&').find_map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (key == name).then(|| percent_decode(value))
        })
    }

    /// The `Content-Length`, or zero when absent or unreadable.
    #[must_use]
    pub fn content_length(&self) -> u64 {
        self.header("content-length")
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0)
    }

    /// A header's value, by case-insensitive name.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        let want = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(n, _)| *n == want)
            .map(|(_, v)| v.as_str())
    }
}

/// Why a request head could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseError {
    /// No blank line yet: the head is not all here.
    Incomplete,
    /// Not HTTP, or a request line this server does not understand.
    Malformed,
    /// Past [`MAX_HEAD`] with no end in sight.
    TooLarge,
}

/// Where the head ends in `buf` (the index just past `\r\n\r\n`), if it does.
#[must_use]
pub fn head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|at| at.saturating_add(4))
}

/// Parse the request head in `buf`.
pub fn parse_request(buf: &[u8]) -> Result<Request, ParseError> {
    let Some(end) = head_end(buf) else {
        return Err(if buf.len() > MAX_HEAD {
            ParseError::TooLarge
        } else {
            ParseError::Incomplete
        });
    };
    let head =
        std::str::from_utf8(buf.get(..end).unwrap_or(&[])).map_err(|_| ParseError::Malformed)?;
    let mut lines = head.split("\r\n");
    let request_line = lines.next().ok_or(ParseError::Malformed)?;
    let mut parts = request_line.split(' ');
    let method = parts.next().ok_or(ParseError::Malformed)?;
    let target = parts.next().ok_or(ParseError::Malformed)?;
    let version = parts.next().ok_or(ParseError::Malformed)?;
    if method.is_empty() || !version.starts_with("HTTP/1.") {
        return Err(ParseError::Malformed);
    }
    let (raw_path, query) = target.split_once('?').unwrap_or((target, ""));
    if !raw_path.starts_with('/') {
        return Err(ParseError::Malformed);
    }
    let headers = lines
        .filter(|line| !line.is_empty())
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            Some((name.trim().to_ascii_lowercase(), value.trim().to_string()))
        })
        .collect();
    Ok(Request {
        method: method.to_ascii_uppercase(),
        path: percent_decode(raw_path),
        query: query.to_string(),
        headers,
    })
}

/// Read one request - the head and, up to `max_body` bytes, the body its
/// `Content-Length` promises - off `stream`. The share reads heads only and
/// has its own loop; this is for the LocalSend side, whose requests carry
/// JSON. A body past `max_body` is [`ParseError::TooLarge`].
pub fn read_request<R: std::io::Read>(
    stream: &mut R,
    max_body: usize,
) -> Result<(Request, Vec<u8>), ParseError> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0_u8; 4096];
    let (request, head_len) = loop {
        match parse_request(&buf) {
            Ok(request) => break (request, head_end(&buf).unwrap_or(buf.len())),
            Err(ParseError::Incomplete) if buf.len() <= MAX_HEAD => {}
            Err(other) => return Err(other),
        }
        match stream.read(&mut chunk) {
            Ok(0) => return Err(ParseError::Malformed),
            Ok(n) => buf.extend_from_slice(chunk.get(..n).unwrap_or(&[])),
            Err(_) => return Err(ParseError::Malformed),
        }
    };
    let want = usize::try_from(request.content_length()).unwrap_or(usize::MAX);
    if want > max_body {
        return Err(ParseError::TooLarge);
    }
    let mut body: Vec<u8> = buf.get(head_len..).unwrap_or(&[]).to_vec();
    while body.len() < want {
        match stream.read(&mut chunk) {
            Ok(0) => return Err(ParseError::Malformed),
            Ok(n) => body.extend_from_slice(chunk.get(..n).unwrap_or(&[])),
            Err(_) => return Err(ParseError::Malformed),
        }
    }
    body.truncate(want);
    Ok((request, body))
}

/// `%41` to `A`, and a `+` left alone: this is a path, not a form.
#[must_use]
pub fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes.get(i).copied().unwrap_or(0);
        if b == b'%' {
            let hex = bytes.get(i.saturating_add(1)..i.saturating_add(3));
            if let Some(v) = hex
                .and_then(|h| std::str::from_utf8(h).ok())
                .and_then(|h| u8::from_str_radix(h, 16).ok())
            {
                out.push(v);
                i = i.saturating_add(3);
                continue;
            }
        }
        out.push(b);
        i = i.saturating_add(1);
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A name made safe for a URL path segment: everything but the unreserved
/// characters and a few that are fine in a path is percent-encoded.
#[must_use]
pub fn percent_encode(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for b in segment.bytes() {
        let keep = b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~');
        if keep {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The bytes a `Range` header asks for, resolved against `len` as an inclusive
/// `(first, last)`. `None` for no header; `Some(Err(()))` for a range that
/// cannot be satisfied, which is a `416`.
///
/// One range only - the first of several is honoured and the rest ignored,
/// which a client is told about by the `Content-Range` it gets back.
#[must_use]
pub fn resolve_range(header: Option<&str>, len: u64) -> Option<Result<(u64, u64), ()>> {
    let spec = header?.trim().strip_prefix("bytes=")?;
    let first = spec.split(',').next().unwrap_or(spec).trim();
    let (start, end) = first.split_once('-')?;
    let parsed = match (start.trim(), end.trim()) {
        // `bytes=-500`: the last 500 bytes.
        ("", suffix) => {
            let n: u64 = suffix.parse().ok()?;
            if n == 0 || len == 0 {
                return Some(Err(()));
            }
            let from = len.saturating_sub(n);
            (from, len.saturating_sub(1))
        }
        // `bytes=500-`: from 500 to the end.
        (from, "") => {
            let from: u64 = from.parse().ok()?;
            if from >= len {
                return Some(Err(()));
            }
            (from, len.saturating_sub(1))
        }
        // `bytes=500-999`.
        (from, to) => {
            let from: u64 = from.parse().ok()?;
            let to: u64 = to.parse().ok()?;
            if from > to || from >= len {
                return Some(Err(()));
            }
            (from, to.min(len.saturating_sub(1)))
        }
    };
    Some(Ok(parsed))
}

/// The content type a file is served as, from its name; a stream of bytes
/// when nothing is known.
#[must_use]
pub fn content_type(path: &Path) -> String {
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    let text = mime.to_string();
    if mime.type_() == mime_guess::mime::TEXT && !text.contains("charset") {
        format!("{text}; charset=utf-8")
    } else {
        text
    }
}

/// A time as HTTP writes it: `Sun, 06 Nov 1994 08:49:37 GMT`.
#[must_use]
pub fn http_date(at: SystemTime) -> String {
    let stamp: chrono::DateTime<chrono::Utc> = at.into();
    stamp.format("%a, %d %b %Y %H:%M:%S GMT").to_string()
}

/// What goes after the head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Body {
    /// Nothing: a `HEAD`, or an error with no page.
    Empty,
    /// These bytes, whole.
    Bytes(Vec<u8>),
    /// `len` bytes of the file at `path`, from `start`. Streamed by the
    /// server so a large file is never read into memory.
    File {
        /// The file to read.
        path: std::path::PathBuf,
        /// The first byte to send.
        start: u64,
        /// How many bytes to send.
        len: u64,
    },
}

/// One response, ready to write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    /// `200`, `206`, `404`, ...
    pub status: u16,
    /// The headers, in order.
    pub headers: Vec<(String, String)>,
    /// The body.
    pub body: Body,
}

impl Response {
    /// A response with the headers every reply carries.
    #[must_use]
    pub fn new(status: u16) -> Self {
        Self {
            status,
            headers: vec![
                (
                    "Server".to_string(),
                    format!("hcmd/{}", env!("CARGO_PKG_VERSION")),
                ),
                ("Date".to_string(), http_date(SystemTime::now())),
                ("Connection".to_string(), "close".to_string()),
                ("Accept-Ranges".to_string(), "bytes".to_string()),
            ],
            body: Body::Empty,
        }
    }

    /// Add a header.
    #[must_use]
    pub fn header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.headers.push((name.to_string(), value.into()));
        self
    }

    /// Give it a body of bytes, with its type and length.
    #[must_use]
    pub fn bytes(mut self, content_type: &str, bytes: Vec<u8>) -> Self {
        self.headers
            .push(("Content-Type".to_string(), content_type.to_string()));
        self.headers
            .push(("Content-Length".to_string(), bytes.len().to_string()));
        self.body = Body::Bytes(bytes);
        self
    }

    /// A short plain-text error page.
    #[must_use]
    pub fn text(status: u16, text: &str) -> Self {
        Self::new(status).bytes(
            "text/plain; charset=utf-8",
            format!("{text}\n").into_bytes(),
        )
    }

    /// The reason phrase for a status this server sends.
    #[must_use]
    pub fn reason(status: u16) -> &'static str {
        match status {
            200 => "OK",
            204 => "No Content",
            206 => "Partial Content",
            207 => "Multi-Status",
            301 => "Moved Permanently",
            400 => "Bad Request",
            403 => "Forbidden",
            404 => "Not Found",
            405 => "Method Not Allowed",
            416 => "Range Not Satisfiable",
            431 => "Request Header Fields Too Large",
            500 => "Internal Server Error",
            _ => "Unknown",
        }
    }

    /// The status line and headers, ending in the blank line.
    #[must_use]
    pub fn head(&self) -> Vec<u8> {
        let mut out = format!("HTTP/1.1 {} {}\r\n", self.status, Self::reason(self.status));
        for (name, value) in &self.headers {
            out.push_str(name);
            out.push_str(": ");
            out.push_str(value);
            out.push_str("\r\n");
        }
        out.push_str("\r\n");
        out.into_bytes()
    }

    /// The same response with no body, for a `HEAD`: the headers - length
    /// included - say what a `GET` would have sent.
    #[must_use]
    pub fn without_body(mut self) -> Self {
        self.body = Body::Empty;
        self
    }
}

/// `&`, `<`, `>` and `"` made safe for HTML.
#[must_use]
pub fn html_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

/// One row of the index page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexEntry {
    /// What is shown.
    pub name: String,
    /// Where it links, already encoded; a directory's ends in `/`.
    pub href: String,
    /// Whether it is a directory.
    pub is_dir: bool,
    /// Size in bytes, for a file.
    pub size: u64,
    /// When it was last changed, when known.
    pub modified: Option<SystemTime>,
}

/// A size the way the index prints it: `1.2 MB`, `812 B`.
#[must_use]
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len().saturating_sub(1) {
        value /= 1024.0;
        unit = unit.saturating_add(1);
    }
    let name = UNITS.get(unit).copied().unwrap_or("B");
    if unit == 0 {
        format!("{bytes} {name}")
    } else {
        format!("{value:.1} {name}")
    }
}

/// A date the way the index prints it: `2026-09-18 14:03`.
#[must_use]
pub fn index_date(at: Option<SystemTime>) -> String {
    at.map_or_else(String::new, |at| {
        let stamp: chrono::DateTime<chrono::Local> = at.into();
        stamp.format("%Y-%m-%d %H:%M").to_string()
    })
}

/// The index page for a directory: a title, a `..` link when there is a
/// parent, then a table of name, size and date, folders first. One page,
/// compiled in, with no script and no outside asset.
#[must_use]
pub fn index_html(title: &str, parent_href: Option<&str>, entries: &[IndexEntry]) -> String {
    let mut rows = String::new();
    if let Some(parent) = parent_href {
        rows.push_str(&format!(
            "<tr><td><a href=\"{}\">..</a></td><td></td><td></td></tr>\n",
            html_escape(parent)
        ));
    }
    let mut sorted: Vec<&IndexEntry> = entries.iter().collect();
    sorted.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    for entry in sorted {
        let shown = if entry.is_dir {
            format!("{}/", entry.name)
        } else {
            entry.name.clone()
        };
        let size = if entry.is_dir {
            String::new()
        } else {
            human_size(entry.size)
        };
        rows.push_str(&format!(
            "<tr><td><a href=\"{}\">{}</a></td><td class=\"n\">{}</td><td>{}</td></tr>\n",
            html_escape(&entry.href),
            html_escape(&shown),
            size,
            index_date(entry.modified)
        ));
    }
    format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>{title}</title>\n\
         <style>\n\
         body{{margin:2rem auto;max-width:60rem;padding:0 1rem;font:15px/1.5 system-ui,sans-serif;color:#1d1d1f;background:#fafafa}}\n\
         h1{{font-size:1.25rem;font-weight:600;margin:0 0 1rem;word-break:break-all}}\n\
         table{{width:100%;border-collapse:collapse}}\n\
         td{{padding:.35rem .5rem;border-top:1px solid #e5e5e7;white-space:nowrap}}\n\
         td:first-child{{white-space:normal;word-break:break-all;width:100%}}\n\
         td.n{{text-align:right;font-variant-numeric:tabular-nums}}\n\
         a{{color:#0b57d0;text-decoration:none}}a:hover{{text-decoration:underline}}\n\
         footer{{margin-top:1.5rem;color:#6e6e73;font-size:.85rem}}\n\
         @media (prefers-color-scheme:dark){{body{{color:#e8e8ed;background:#1c1c1e}}td{{border-color:#3a3a3c}}a{{color:#6cb4ff}}footer{{color:#98989d}}}}\n\
         </style></head>\n<body>\n<h1>{title}</h1>\n<table>\n{rows}</table>\n\
         <footer>served by Holos Commander</footer>\n</body></html>\n",
        title = html_escape(title),
        rows = rows
    )
}

#[cfg(test)]
#[path = "http_tests.rs"]
mod tests;
