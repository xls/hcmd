//! The little client against a scripted socket.

use super::*;
use std::io::{Cursor, Write};

/// A stream that answers with `reply` and keeps what was written.
struct Scripted {
    reply: Cursor<Vec<u8>>,
    written: Vec<u8>,
}

impl Read for Scripted {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.reply.read(buf)
    }
}

impl Write for Scripted {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.written.extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn scripted(reply: &str) -> Scripted {
    Scripted {
        reply: Cursor::new(reply.as_bytes().to_vec()),
        written: Vec::new(),
    }
}

/// A stream that answers with `reply` in reads of at most `piece` bytes and
/// then, instead of closing, errors: a server that keeps the connection
/// open. A reader that waits for the close would see this error.
struct KeptOpen {
    reply: Vec<u8>,
    at: usize,
    piece: usize,
}

impl Read for KeptOpen {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.at >= self.reply.len() {
            return Err(std::io::Error::other("would hang: read past the body"));
        }
        let n = self.piece.min(buf.len()).min(self.reply.len() - self.at);
        buf[..n].copy_from_slice(&self.reply[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

impl Write for KeptOpen {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

const CHUNKED_ACCEPT: &str = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n\
    10\r\n{\"sessionId\":\"s\"\r\n\
    13\r\n,\"files\":{\"a\":\"t\"}}\r\n\
    0\r\n\r\n";

#[test]
fn a_chunked_answer_on_a_connection_the_server_keeps_open_ends_at_the_last_chunk() {
    let mut s = KeptOpen {
        reply: CHUNKED_ACCEPT.as_bytes().to_vec(),
        at: 0,
        piece: 4096,
    };
    let response = request(&mut s, "h", "POST", "/x", Body::None, &mut |_| true)
        .expect("the answer, without waiting for a close");
    assert_eq!(response.status, 200);
    assert_eq!(
        response.text(),
        "{\"sessionId\":\"s\",\"files\":{\"a\":\"t\"}}"
    );
}

#[test]
fn a_chunked_body_arriving_in_small_pieces_is_put_back_together() {
    for piece in 1..=7 {
        let mut s = KeptOpen {
            reply: CHUNKED_ACCEPT.as_bytes().to_vec(),
            at: 0,
            piece,
        };
        let response = request(&mut s, "h", "POST", "/x", Body::None, &mut |_| true)
            .unwrap_or_else(|e| panic!("pieces of {piece}: {e}"));
        assert_eq!(
            response.text(),
            "{\"sessionId\":\"s\",\"files\":{\"a\":\"t\"}}",
            "pieces of {piece}"
        );
    }
}

/// A stream that hands out `reply` and then reports the close the stock
/// server does: an `UnexpectedEof` (rustls' "closed without close_notify").
struct NoCloseNotify {
    reply: Cursor<Vec<u8>>,
}

impl Read for NoCloseNotify {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self.reply.read(buf)? {
            0 => Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "peer closed connection without sending TLS close_notify",
            )),
            n => Ok(n),
        }
    }
}

impl Write for NoCloseNotify {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn no_close_notify(reply: &str) -> NoCloseNotify {
    NoCloseNotify {
        reply: Cursor::new(reply.as_bytes().to_vec()),
    }
}

#[test]
fn a_close_without_close_notify_after_the_body_is_the_end_not_an_error() {
    // Chunked, ended by its terminal chunk, then the abrupt close.
    let mut s = no_close_notify(CHUNKED_ACCEPT);
    let response = request(&mut s, "h", "POST", "/x", Body::None, &mut |_| true).expect("chunked");
    assert_eq!(
        response.text(),
        "{\"sessionId\":\"s\",\"files\":{\"a\":\"t\"}}"
    );
    // Length-delimited, then the abrupt close.
    let mut s = no_close_notify("HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
    let response = request(&mut s, "h", "GET", "/x", Body::None, &mut |_| true).expect("length");
    assert_eq!(response.text(), "ok");
    // Neither: read to the close, and the abrupt close is the close.
    let mut s = no_close_notify("HTTP/1.1 500 Oops\r\n\r\nit broke");
    let response = request(&mut s, "h", "GET", "/x", Body::None, &mut |_| true).expect("to close");
    assert_eq!(
        (response.status, response.text().as_str()),
        (500, "it broke")
    );
    // But before any status line, the abrupt close is still a failure.
    let mut s = no_close_notify("");
    assert!(request(&mut s, "h", "GET", "/x", Body::None, &mut |_| true).is_err());
}

#[test]
fn a_100_continue_before_the_real_answer_is_skipped() {
    let mut s =
        scripted("HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
    let response = request(
        &mut s,
        "h",
        "POST",
        "/x",
        Body::Bytes(b"{}", "application/json"),
        &mut |_| true,
    )
    .expect("answer");
    assert_eq!((response.status, response.text().as_str()), (200, "ok"));
}

#[test]
fn a_chunked_body_cut_off_by_a_close_is_an_error_not_a_partial_answer() {
    let mut s = scripted("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhel");
    let err = request(&mut s, "h", "POST", "/x", Body::None, &mut |_| true).expect_err("cut");
    assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
}

#[test]
fn a_json_post_writes_a_length_delimited_request_and_reads_the_answer() {
    let mut s = scripted(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 13\r\n\r\n{\"ok\":\"yes\"}\n",
    );
    let response = request(
        &mut s,
        "10.0.0.5:53317",
        "POST",
        "/api/localsend/v2/prepare-upload?pin=1234",
        Body::Bytes(b"{\"a\":1}", "application/json"),
        &mut |_| true,
    )
    .expect("answer");
    assert_eq!(response.status, 200);
    assert_eq!(response.header("content-type"), Some("application/json"));
    assert_eq!(response.text(), "{\"ok\":\"yes\"}\n");
    let sent = String::from_utf8(s.written).expect("text");
    assert!(
        sent.starts_with("POST /api/localsend/v2/prepare-upload?pin=1234 HTTP/1.1\r\n"),
        "{sent}"
    );
    assert!(sent.contains("\r\nHost: 10.0.0.5:53317\r\n"), "{sent}");
    assert!(sent.contains("\r\nContent-Length: 7\r\n"), "{sent}");
    assert!(
        sent.contains("\r\nContent-Type: application/json\r\n"),
        "{sent}"
    );
    assert!(sent.ends_with("\r\n\r\n{\"a\":1}"), "{sent}");
}

#[test]
fn a_streamed_body_goes_out_with_its_length_and_reports_progress() {
    let mut s = scripted("HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
    let payload: Vec<u8> = (0..200_000_u32).map(|i| (i % 251) as u8).collect();
    let mut reader = Cursor::new(payload.clone());
    let mut reported = 0_u64;
    let response = request(
        &mut s,
        "h",
        "POST",
        "/api/localsend/v2/upload?sessionId=s&fileId=f&token=t",
        Body::Stream(&mut reader, payload.len() as u64),
        &mut |n| {
            reported += n;
            true
        },
    )
    .expect("answer");
    assert_eq!(response.status, 200);
    assert_eq!(reported, payload.len() as u64);
    let head_end = s
        .written
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("head")
        + 4;
    let head = String::from_utf8_lossy(&s.written[..head_end]).into_owned();
    assert!(head.contains("Content-Length: 200000\r\n"), "{head}");
    assert!(!head.contains("chunked"), "{head}");
    assert_eq!(
        &s.written[head_end..],
        &payload[..],
        "the body is the file, byte for byte"
    );
}

#[test]
fn saying_no_to_progress_stops_the_upload_as_an_interruption() {
    let mut s = scripted("HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
    let payload = vec![7_u8; 300_000];
    let mut reader = Cursor::new(payload);
    let mut calls = 0;
    let err = request(
        &mut s,
        "h",
        "POST",
        "/x",
        Body::Stream(&mut reader, 300_000),
        &mut |_| {
            calls += 1;
            calls < 2
        },
    )
    .expect_err("stopped");
    assert_eq!(err.kind(), std::io::ErrorKind::Interrupted);
    assert_eq!(calls, 2);
}

#[test]
fn a_file_shorter_than_its_size_said_is_an_error_not_a_hang() {
    let mut s = scripted("HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
    let mut reader = Cursor::new(vec![1_u8; 10]);
    let err = request(
        &mut s,
        "h",
        "POST",
        "/x",
        Body::Stream(&mut reader, 20),
        &mut |_| true,
    )
    .expect_err("short");
    assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
}

#[test]
fn a_chunked_answer_is_put_back_together() {
    let mut s = scripted(
        "HTTP/1.1 403 Forbidden\r\nTransfer-Encoding: chunked\r\n\r\n4\r\nnope\r\n3\r\n, n\r\n2\r\no.\r\n0\r\n\r\n",
    );
    let response = request(&mut s, "h", "POST", "/x", Body::None, &mut |_| true).expect("answer");
    assert_eq!(response.status, 403);
    assert_eq!(response.text(), "nope, no.");
}

#[test]
fn an_answer_without_a_length_is_read_to_the_close_and_a_closed_socket_is_said() {
    let mut s = scripted("HTTP/1.1 500 Oops\r\n\r\nit broke");
    let response = request(&mut s, "h", "GET", "/x", Body::None, &mut |_| true).expect("answer");
    assert_eq!(
        (response.status, response.text().as_str()),
        (500, "it broke")
    );

    let mut s = scripted("");
    let err = request(&mut s, "h", "GET", "/x", Body::None, &mut |_| true).expect_err("closed");
    assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);

    let mut s = scripted("I am not HTTP\r\n\r\n");
    let err = request(&mut s, "h", "GET", "/x", Body::None, &mut |_| true).expect_err("junk");
    assert!(err.to_string().contains("not an HTTP answer"), "{err}");
}
