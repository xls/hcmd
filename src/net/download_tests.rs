use super::*;

#[test]
fn a_fresh_get_writes_the_whole_file_from_the_start() {
    // No resume offset, an ordinary 200: rewrite from zero with the stated
    // size.
    assert_eq!(
        plan(0, 200, Some(1000), None).expect("200 is fine"),
        Plan::Write {
            offset: 0,
            total: Some(1000)
        }
    );
}

#[test]
fn an_honoured_range_continues_from_the_offset() {
    // 206 with a partial length: the finished size is the offset plus what is
    // still to come.
    assert_eq!(
        plan(200, 206, Some(800), None).expect("206 is fine"),
        Plan::Write {
            offset: 200,
            total: Some(1000)
        }
    );
    // And a Content-Range total is trusted over the arithmetic when present.
    assert_eq!(
        plan(200, 206, Some(800), Some(1000)).expect("206 is fine"),
        Plan::Write {
            offset: 200,
            total: Some(1000)
        }
    );
}

#[test]
fn a_server_that_ignores_the_range_restarts_the_file() {
    // We asked to resume at 200, but the server answered 200 with the whole
    // file: the half-download is discarded rather than appended to.
    assert_eq!(
        plan(200, 200, Some(1000), None).expect("200 is fine"),
        Plan::Write {
            offset: 0,
            total: Some(1000)
        }
    );
}

#[test]
fn a_satisfied_range_is_nothing_to_fetch() {
    // 416: the partial file is already the whole resource.
    assert_eq!(
        plan(1000, 416, None, None).expect("416 is complete"),
        Plan::Complete
    );
}

#[test]
fn a_failure_status_is_an_error_not_a_write() {
    assert!(plan(0, 404, None, None).is_err());
    assert!(plan(0, 403, None, None).is_err());
    assert!(plan(200, 500, None, None).is_err());
}

#[test]
fn the_content_range_total_is_read_after_the_slash() {
    assert_eq!(range_total("bytes 0-99/1000"), Some(1000));
    assert_eq!(range_total("bytes 200-999/1000"), Some(1000));
    // An unknown total is not a size.
    assert_eq!(range_total("bytes 0-99/*"), None);
    // Garbage is not a size either.
    assert_eq!(range_total("nonsense"), None);
    assert_eq!(range_total(""), None);
}

/// A local HTTP server over a scratch directory, for an end-to-end transfer
/// with no outside network. Python's `http.server` does not honour `Range`, so
/// it doubles as the "server ignored the range" case.
struct LocalServer {
    child: std::process::Child,
    port: u16,
}

impl LocalServer {
    fn serve(dir: &std::path::Path) -> Self {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .expect("a free port")
            .local_addr()
            .expect("its address")
            .port();
        let mut child = std::process::Command::new("python3")
            .args([
                "-m",
                "http.server",
                &port.to_string(),
                "--bind",
                "127.0.0.1",
            ])
            .current_dir(dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("python3 http.server starts");
        // Wait until it answers rather than racing it.
        for _ in 0..100 {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                return Self { child, port };
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        // Reap it before giving up, so a server that never answered is not
        // left as a zombie behind the panic.
        let _ = child.kill();
        let _ = child.wait();
        panic!("the local server never came up on port {port}");
    }

    fn url(&self, name: &str) -> String {
        format!("http://127.0.0.1:{}/{name}", self.port)
    }
}

impl Drop for LocalServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
#[ignore = "spawns python3 http.server on localhost; run with: cargo test -- --ignored end_to_end"]
fn end_to_end_a_file_is_streamed_to_disk_and_a_partial_one_is_rewritten() {
    let dir = std::env::temp_dir().join(format!("hcmd-dl-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    // Bigger than one chunk, so the loop actually loops.
    let payload: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(dir.join("blob.bin"), &payload).expect("the served file");
    let server = LocalServer::serve(&dir);
    let dest = dir.join("out.bin");

    // A fresh download lands byte for byte, and progress reported the total
    // before the first chunk and climbed to it.
    let mut reports: Vec<(u64, Option<u64>)> = Vec::new();
    let status = download(&server.url("blob.bin"), &dest, false, &mut |done, total| {
        reports.push((done, total));
        true
    })
    .expect("the download succeeds");
    assert_eq!(status, DownloadStatus::Completed);
    assert_eq!(std::fs::read(&dest).expect("out.bin"), payload);
    assert_eq!(
        reports.first(),
        Some(&(0, Some(payload.len() as u64))),
        "{reports:?}"
    );
    assert_eq!(reports.last().map(|r| r.0), Some(payload.len() as u64));

    // Cut the file in half and ask to resume. This server ignores Range and
    // answers 200 with everything, which must rewrite the file cleanly rather
    // than append a second copy.
    std::fs::write(&dest, &payload[..100_000]).expect("truncate");
    let status = download(&server.url("blob.bin"), &dest, true, &mut |_, _| true)
        .expect("the resume attempt succeeds");
    assert_eq!(status, DownloadStatus::Completed);
    assert_eq!(
        std::fs::read(&dest).expect("out.bin"),
        payload,
        "not appended, rewritten"
    );

    // A cancel from the callback stops early and leaves a partial file.
    let status = download(&server.url("blob.bin"), &dest, false, &mut |done, _| {
        done == 0
    })
    .expect("a cancel is not an error");
    assert_eq!(status, DownloadStatus::Cancelled);
    assert!(
        std::fs::metadata(&dest).expect("partial").len() < payload.len() as u64,
        "the partial file is left in place"
    );

    drop(server);
    let _ = std::fs::remove_dir_all(&dir);
}
