//! Just enough HTTP/1.1 client to speak LocalSend over a stream we own.
//!
//! The requests are four, the bodies are small JSON or one file streamed
//! with a `Content-Length`, and the connection is ours because the TLS layer
//! pins a fingerprint no general-purpose client can be told about. Writing a
//! request line and a few headers and reading a status line back is less
//! code than teaching a client library to do it.

use std::io::Read;

/// What goes out with a request.
pub enum Body<'a> {
    /// Nothing.
    None,
    /// These bytes, with their `Content-Type`.
    Bytes(&'a [u8], &'static str),
    /// `len` bytes read from `reader`, streamed; `Content-Length` says `len`.
    Stream(&'a mut dyn Read, u64),
}

/// A response: the status and, up to a limit, the body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    /// The status code.
    pub status: u16,
    /// The headers, names lower-cased.
    pub headers: Vec<(String, String)>,
    /// The body, whole.
    pub body: Vec<u8>,
}

impl Response {
    /// A header's value, by case-insensitive name.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// The body as text, lossily.
    #[must_use]
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// The most head this side will read.
const MAX_HEAD: usize = 64 * 1024;
/// The most body this side will read: a `prepare-upload` answer is a few
/// hundred bytes, an error page a few kilobytes.
const MAX_BODY: usize = 1024 * 1024;
/// The chunk the streamed body is copied in.
const CHUNK: usize = 64 * 1024;

/// Send `method target` with `body` on `stream` and read the answer.
///
/// `progress` is called with each chunk of a streamed body as it goes out
/// and may return `false` to stop; the caller then sees
/// [`std::io::ErrorKind::Interrupted`].
pub fn request(
    stream: &mut dyn super::tls::Stream,
    host: &str,
    method: &str,
    target: &str,
    body: Body<'_>,
    progress: &mut dyn FnMut(u64) -> bool,
) -> std::io::Result<Response> {
    let mut head = format!("{method} {target} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n");
    head.push_str(&format!("User-Agent: {}\r\n", crate::net::agent()));
    match &body {
        Body::None => head.push_str("Content-Length: 0\r\n"),
        Body::Bytes(bytes, content_type) => {
            head.push_str(&format!("Content-Type: {content_type}\r\n"));
            head.push_str(&format!("Content-Length: {}\r\n", bytes.len()));
        }
        Body::Stream(_, len) => {
            head.push_str("Content-Type: application/octet-stream\r\n");
            head.push_str(&format!("Content-Length: {len}\r\n"));
        }
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    match body {
        Body::None => {}
        Body::Bytes(bytes, _) => stream.write_all(bytes)?,
        Body::Stream(reader, len) => {
            let mut buf = vec![0_u8; CHUNK];
            let mut left = len;
            while left > 0 {
                let want = usize::try_from(left.min(CHUNK as u64)).unwrap_or(CHUNK);
                let n = reader.read(buf.get_mut(..want).unwrap_or(&mut []))?;
                if n == 0 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "the file ended before its size said it would",
                    ));
                }
                stream.write_all(buf.get(..n).unwrap_or(&[]))?;
                left = left.saturating_sub(n as u64);
                if !progress(n as u64) {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Interrupted,
                        "cancelled",
                    ));
                }
            }
        }
    }
    stream.flush()?;
    read_response(stream)
}

/// Read a response off `stream`.
///
/// The body ends where HTTP says it ends - at `Content-Length` bytes, or at
/// the terminal chunk of a chunked body - and never at the close of the
/// connection when either of those is known. A server that keeps the
/// connection open despite `Connection: close` (Dart's does) would otherwise
/// leave this side waiting for a close that never comes, which is the whole
/// transfer sitting still after the other side tapped Accept. Only a body
/// with neither length nor chunking is read to the close.
fn read_response(stream: &mut dyn Read) -> std::io::Result<Response> {
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut chunk = [0_u8; 4096];
    let (status, headers, head_end) = loop {
        let head_end = read_head(stream, &mut buf, &mut chunk)?;
        let (status, headers) = parse_head(buf.get(..head_end).unwrap_or(&[]))?;
        // A `100 Continue` is the server clearing its throat before the real
        // answer: drop it and read the next head.
        if status == 100 {
            buf.drain(..head_end);
            continue;
        }
        break (status, headers, head_end);
    };
    let mut body: Vec<u8> = buf.get(head_end..).unwrap_or(&[]).to_vec();
    let chunked = headers
        .iter()
        .any(|(n, v)| n == "transfer-encoding" && v.to_ascii_lowercase().contains("chunked"));
    let length: Option<usize> = headers
        .iter()
        .find(|(n, _)| n == "content-length")
        .and_then(|(_, v)| v.parse().ok());
    if chunked {
        body = read_chunked(stream, body, &mut chunk)?;
    } else if let Some(len) = length {
        if len > MAX_BODY {
            return Err(std::io::Error::other("response body too large"));
        }
        while body.len() < len {
            let n = read_or_end(stream, &mut chunk)?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(chunk.get(..n).unwrap_or(&[]));
        }
        body.truncate(len);
    } else {
        read_to_end_bounded(stream, &mut body)?;
    }
    Ok(Response {
        status,
        headers,
        body,
    })
}

/// One read, with the close a LocalSend server does taken as the close it
/// is: the stock server shuts the socket without a TLS `close_notify`, which
/// rustls reports as `UnexpectedEof`. Past the status line that is the end
/// of the stream, not a failure - the callers decide whether an end there
/// is a complete body.
fn read_or_end(stream: &mut dyn Read, buf: &mut [u8]) -> std::io::Result<usize> {
    match stream.read(buf) {
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(0),
        other => other,
    }
}

/// Read until `buf` holds a whole head; the index just past its blank line.
fn read_head(
    stream: &mut dyn Read,
    buf: &mut Vec<u8>,
    chunk: &mut [u8; 4096],
) -> std::io::Result<usize> {
    loop {
        if let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            return Ok(at.saturating_add(4));
        }
        if buf.len() > MAX_HEAD {
            return Err(std::io::Error::other("response head too large"));
        }
        let n = stream.read(chunk)?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "the device closed the connection before answering",
            ));
        }
        buf.extend_from_slice(chunk.get(..n).unwrap_or(&[]));
    }
}

/// The status and headers of one head.
fn parse_head(head: &[u8]) -> std::io::Result<(u16, Vec<(String, String)>)> {
    let head = String::from_utf8_lossy(head).into_owned();
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or("");
    let status: u16 = status_line
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| std::io::Error::other(format!("not an HTTP answer: {status_line:?}")))?;
    let headers = lines
        .filter(|l| !l.is_empty())
        .filter_map(|l| {
            let (n, v) = l.split_once(':')?;
            Some((n.trim().to_ascii_lowercase(), v.trim().to_string()))
        })
        .collect();
    Ok((status, headers))
}

/// Read a chunked body, starting from what `raw` already holds, and stop at
/// the terminal chunk: nothing past it is read, so a connection the server
/// keeps open costs nothing.
fn read_chunked(
    stream: &mut dyn Read,
    mut raw: Vec<u8>,
    chunk: &mut [u8; 4096],
) -> std::io::Result<Vec<u8>> {
    loop {
        match dechunk(&raw)? {
            Chunked::Done(body) => return Ok(body),
            Chunked::Incomplete => {}
        }
        if raw.len() > MAX_BODY {
            return Err(std::io::Error::other("response body too large"));
        }
        let n = read_or_end(stream, chunk)?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "the device closed the connection mid-body",
            ));
        }
        raw.extend_from_slice(chunk.get(..n).unwrap_or(&[]));
    }
}

/// Read until the other side closes, or the limit.
fn read_to_end_bounded(stream: &mut dyn Read, into: &mut Vec<u8>) -> std::io::Result<()> {
    let mut chunk = [0_u8; 4096];
    loop {
        if into.len() > MAX_BODY {
            return Err(std::io::Error::other("response body too large"));
        }
        let n = read_or_end(stream, &mut chunk)?;
        if n == 0 {
            return Ok(());
        }
        into.extend_from_slice(chunk.get(..n).unwrap_or(&[]));
    }
}

/// What [`dechunk`] made of the bytes so far.
enum Chunked {
    /// The whole body: the terminal chunk has been seen.
    Done(Vec<u8>),
    /// More bytes are needed.
    Incomplete,
}

/// Undo `Transfer-Encoding: chunked` over `raw`, as far as it goes.
///
/// `Incomplete` when the terminal chunk has not arrived yet; an error only
/// for bytes that cannot be chunked encoding at all.
fn dechunk(raw: &[u8]) -> std::io::Result<Chunked> {
    let mut out = Vec::with_capacity(raw.len());
    let mut at = 0_usize;
    loop {
        let rest = raw.get(at..).unwrap_or(&[]);
        let Some(line_end) = rest.windows(2).position(|w| w == b"\r\n") else {
            return Ok(Chunked::Incomplete);
        };
        let size_text = String::from_utf8_lossy(rest.get(..line_end).unwrap_or(&[])).into_owned();
        let size = usize::from_str_radix(size_text.split(';').next().unwrap_or("").trim(), 16)
            .map_err(|_| std::io::Error::other("bad chunk size"))?;
        at = at.saturating_add(line_end).saturating_add(2);
        if size == 0 {
            // Trailers, if any, up to the blank line; absent trailers are
            // just the blank line, which need not have arrived yet.
            return Ok(Chunked::Done(out));
        }
        let Some(data) = raw.get(at..at.saturating_add(size)) else {
            return Ok(Chunked::Incomplete);
        };
        out.extend_from_slice(data);
        at = at.saturating_add(size).saturating_add(2);
    }
}

#[cfg(test)]
#[path = "http_tests.rs"]
mod tests;
