//! The share end to end, against our own client and a raw socket.

use std::io::{Read, Write};
use std::net::TcpStream;

use super::*;
use crate::net::{DownloadStatus, download};

/// A scratch share: a folder with a file and a subfolder, and a lone file.
fn scratch(tag: &str) -> (std::path::PathBuf, Vec<Root>) {
    let dir = std::env::temp_dir().join(format!("hcmd-serve-e2e-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("photos/2026")).expect("dirs");
    let payload: Vec<u8> = (0..150_000_u32).map(|i| (i % 253) as u8).collect();
    std::fs::write(dir.join("photos/big.bin"), &payload).expect("file");
    std::fs::write(dir.join("photos/2026/note.txt"), b"hello\n").expect("file");
    std::fs::write(dir.join("report.pdf"), b"%PDF-1.4\n").expect("file");
    let roots = tree::roots(&[dir.join("photos"), dir.join("report.pdf")]);
    (dir, roots)
}

/// A server over the scratch share on localhost, and the events it sends.
fn serve(roots: Vec<Root>) -> (Server, mpsc::Receiver<ServeEvent>) {
    let (tx, rx) = mpsc::channel(SERVE_CHANNEL_DEPTH);
    let server = Server::start_on("127.0.0.1:0", roots, tx).expect("a listener");
    (server, rx)
}

/// One raw request, and everything the server sent back.
fn raw(port: u16, request: &str) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stream.write_all(request.as_bytes()).expect("write");
    let mut out = Vec::new();
    stream.read_to_end(&mut out).expect("read");
    String::from_utf8_lossy(&out).into_owned()
}

#[test]
fn a_file_is_fetched_whole_and_resumed_by_our_own_client() {
    let (dir, roots) = scratch("fetch");
    let (server, mut rx) = serve(roots);
    let url = format!("http://127.0.0.1:{}/photos/big.bin", server.port());
    let dest = dir.join("out.bin");
    let want = std::fs::read(dir.join("photos/big.bin")).expect("payload");

    // Whole.
    let status = download(&url, &dest, false, &mut |_, _| true).expect("fetch");
    assert_eq!(status, DownloadStatus::Completed);
    assert_eq!(std::fs::read(&dest).expect("out"), want);

    // Cut in half and resumed: the server honours Range, so only the rest
    // travels and the file comes out whole.
    std::fs::write(&dest, &want[..70_000]).expect("truncate");
    let mut reports = Vec::new();
    let status = download(&url, &dest, true, &mut |done, total| {
        reports.push((done, total));
        true
    })
    .expect("resume");
    assert_eq!(status, DownloadStatus::Completed);
    assert_eq!(std::fs::read(&dest).expect("out"), want);
    assert_eq!(
        reports.first(),
        Some(&(70_000, Some(want.len() as u64))),
        "the resume started where the file stopped: {reports:?}"
    );

    // Both requests were logged, the second as a 206.
    let mut seen = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let ServeEvent::Request(served) = event {
            seen.push((served.method, served.status));
        }
    }
    assert!(seen.contains(&("GET".to_string(), 200)), "{seen:?}");
    assert!(seen.contains(&("GET".to_string(), 206)), "{seen:?}");

    drop(server);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_index_lists_the_selection_and_a_folder_lists_its_files() {
    let (dir, roots) = scratch("index");
    let (server, _rx) = serve(roots);
    let base = format!("http://127.0.0.1:{}", server.port());

    let root = crate::net::get_text(&format!("{base}/")).expect("index");
    assert!(root.contains("photos/"), "{root}");
    assert!(root.contains("report.pdf"), "{root}");

    // A folder without its slash is sent to the slash, and with it lists.
    let redirect = raw(server.port(), "GET /photos HTTP/1.1\r\nHost: x\r\n\r\n");
    assert!(redirect.starts_with("HTTP/1.1 301"), "{redirect}");
    assert!(redirect.contains("Location: /photos/\r\n"), "{redirect}");
    let folder = crate::net::get_text(&format!("{base}/photos/")).expect("folder");
    assert!(folder.contains("big.bin"), "{folder}");
    assert!(folder.contains("2026/"), "{folder}");
    assert!(
        folder.contains("href=\"/\">..</a>"),
        "under a root, up is the index: {folder}"
    );
    let deeper = crate::net::get_text(&format!("{base}/photos/2026/")).expect("deeper");
    assert!(deeper.contains("href=\"../\">..</a>"), "{deeper}");

    drop(server);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_webdav_client_can_list_the_share_and_writes_are_refused() {
    let (dir, roots) = scratch("dav");
    let (server, _rx) = serve(roots);
    let port = server.port();

    let options = raw(port, "OPTIONS / HTTP/1.1\r\nHost: x\r\n\r\n");
    assert!(options.starts_with("HTTP/1.1 204"), "{options}");
    assert!(options.contains("DAV: 1\r\n"), "{options}");
    assert!(
        options.contains("Allow: OPTIONS, GET, HEAD, PROPFIND\r\n"),
        "{options}"
    );

    let listing = raw(port, "PROPFIND / HTTP/1.1\r\nHost: x\r\nDepth: 1\r\n\r\n");
    assert!(listing.starts_with("HTTP/1.1 207"), "{listing}");
    assert!(listing.contains("<D:href>/photos/</D:href>"), "{listing}");
    assert!(
        listing.contains("<D:href>/report.pdf</D:href>"),
        "{listing}"
    );
    assert!(listing.contains("<D:collection/>"), "{listing}");

    let one = raw(
        port,
        "PROPFIND /photos/big.bin HTTP/1.1\r\nHost: x\r\nDepth: 0\r\n\r\n",
    );
    assert!(
        one.contains("<D:getcontentlength>150000</D:getcontentlength>"),
        "{one}"
    );
    assert!(
        one.contains("<D:getcontenttype>application/octet-stream</D:getcontenttype>"),
        "{one}"
    );

    // Read-only, and it says so with what it does allow.
    let put = raw(
        port,
        "PUT /photos/new.txt HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\n\r\n",
    );
    assert!(put.starts_with("HTTP/1.1 405"), "{put}");
    assert!(
        put.contains("Allow: OPTIONS, GET, HEAD, PROPFIND\r\n"),
        "{put}"
    );

    drop(server);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn head_range_and_the_refusals_answer_as_http_says() {
    let (dir, roots) = scratch("head");
    let (server, _rx) = serve(roots);
    let port = server.port();

    let head = raw(port, "HEAD /photos/big.bin HTTP/1.1\r\nHost: x\r\n\r\n");
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert!(head.contains("Content-Length: 150000\r\n"), "{head}");
    assert!(head.ends_with("\r\n\r\n"), "no body after a HEAD: {head:?}");

    let range = raw(
        port,
        "GET /photos/2026/note.txt HTTP/1.1\r\nHost: x\r\nRange: bytes=1-3\r\n\r\n",
    );
    assert!(range.starts_with("HTTP/1.1 206"), "{range}");
    assert!(range.contains("Content-Range: bytes 1-3/6\r\n"), "{range}");
    assert!(range.ends_with("\r\n\r\nell"), "{range:?}");

    let bad = raw(
        port,
        "GET /photos/2026/note.txt HTTP/1.1\r\nHost: x\r\nRange: bytes=99-\r\n\r\n",
    );
    assert!(bad.starts_with("HTTP/1.1 416"), "{bad}");

    let escape = raw(
        port,
        "GET /photos/../../etc/passwd HTTP/1.1\r\nHost: x\r\n\r\n",
    );
    assert!(escape.starts_with("HTTP/1.1 403"), "{escape}");
    let missing = raw(port, "GET /nothing HTTP/1.1\r\nHost: x\r\n\r\n");
    assert!(missing.starts_with("HTTP/1.1 404"), "{missing}");
    let junk = raw(port, "hello there\r\n\r\n");
    assert!(junk.starts_with("HTTP/1.1 400"), "{junk}");

    drop(server);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn stopping_the_server_closes_the_port() {
    let (dir, roots) = scratch("stop");
    let (mut server, _rx) = serve(roots);
    let port = server.port();
    assert!(
        TcpStream::connect(("127.0.0.1", port)).is_ok(),
        "open while serving"
    );
    server.stop();
    // The listener is gone: a fresh connect is refused.
    let refused = (0..20).any(|_| {
        std::thread::sleep(std::time::Duration::from_millis(20));
        TcpStream::connect(("127.0.0.1", port)).is_err()
    });
    assert!(refused, "the port closed once the dialog would have");
    let _ = std::fs::remove_dir_all(&dir);
}
