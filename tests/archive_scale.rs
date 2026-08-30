//! The measurement behind the streaming claim.
//!
//! > A 4 GB `.tar.gz` must not be decompressed in full to list it; build the
//! > index in the background and populate the panel as entries appear.
//!
//! That is a statement about memory and about latency, and neither can be
//! checked by reading the code: a `Read` chain that looks like a stream still
//! buffers the whole thing if one link in it collects. So this listens to what
//! the panel listens to - the `read_dir` receiver - and times the first row
//! that is not `..`, while watching the process's own resident set.
//!
//! Both fixtures are large and slow to build, so they are cached between runs
//! and both tests are `#[ignore]`d: `cargo test` stays fast and this is run
//! deliberately.
//!
//! ```text
//! cargo test --release --test archive_scale -- --ignored --nocapture
//! ```
//!
//! `HCMD_SCALE_DIR` moves the fixtures; it must not be a tmpfs, or the
//! fixtures themselves are counted as memory.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "an integration test is its own crate, so it is not #[cfg(test)] \
              and clippy.toml's allow-*-in-tests keys do not reach it. \
              Panicking assertions are the point of a test."
)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use holoscommander::vfs::archive::{ArchiveSession, RewriteLimits};
use holoscommander::vfs::{BackendKind, Vfs, VfsPath};

/// Where the cached fixtures live. Not `/tmp`: it is a tmpfs on this machine,
/// and a 4 GB fixture in RAM makes nonsense of an RSS measurement.
fn fixture_dir() -> PathBuf {
    let dir = std::env::var_os("HCMD_SCALE_DIR").map_or_else(
        || {
            let base = std::env::var_os("HOME").map_or_else(
                || PathBuf::from("."),
                |home| PathBuf::from(home).join(".cache"),
            );
            base.join("hcmd-scale")
        },
        PathBuf::from,
    );
    std::fs::create_dir_all(&dir).expect("the fixture directory");
    dir
}

/// One line of `/proc/self/status`, in bytes.
fn proc_status_kib(field: &str) -> u64 {
    let Ok(text) = std::fs::read_to_string("/proc/self/status") else {
        return 0;
    };
    text.lines()
        .find(|line| line.starts_with(field))
        .and_then(|line| {
            line.split_whitespace()
                .nth(1)
                .and_then(|n| n.parse::<u64>().ok())
        })
        .unwrap_or(0)
}

/// Resident set now.
fn rss_mib() -> f64 {
    proc_status_kib("VmRSS:") as f64 / 1024.0
}

/// The high-water mark the kernel has recorded for this process.
fn peak_rss_mib() -> f64 {
    proc_status_kib("VmHWM:") as f64 / 1024.0
}

fn inside(container: &Path, member: &str) -> VfsPath {
    VfsPath::local(container).with_segment(BackendKind::Archive, member)
}

fn session() -> Arc<ArchiveSession> {
    ArchiveSession::in_dir(fixture_dir().join("cache"), RewriteLimits::default())
        .expect("the session")
}

/// A 4 GiB `.tar.gz`: 4096 members of 1 MiB each.
///
/// The bodies are compressible on purpose, so the fixture is a few megabytes
/// on disk while still being four gigabytes of tar to walk. That is the shape
/// the design names, and it is also the shape that catches a reader that
/// decompresses to a buffer: on disk it is small, in memory it is not.
fn four_gigabyte_targz() -> PathBuf {
    let path = fixture_dir().join("four-gib.tar.gz");
    if path.exists() {
        return path;
    }
    eprintln!("building {} (4 GiB of tar) ...", path.display());
    let file = std::fs::File::create(&path).expect("create");
    let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
    let mut builder = tar::Builder::new(encoder);
    // Not zeroes: an all-zero body compresses so hard that the gzip stream
    // stops being a realistic amount of work to decompress. Text with
    // structure is closer to a real archive and still compresses well.
    let mut body = Vec::with_capacity(1024 * 1024);
    let mut n: u64 = 0;
    while body.len() < 1024 * 1024 {
        let _ = writeln!(body, "line {n} of a file that exists to take up room");
        n = n.wrapping_add(1);
    }
    body.truncate(1024 * 1024);
    for i in 0..4096 {
        let mut header = tar::Header::new_gnu();
        header.set_size(body.len() as u64);
        header.set_mode(0o644);
        header.set_mtime(1_700_000_000);
        header.set_cksum();
        builder
            .append_data(
                &mut header,
                format!("d{:03}/member-{i:04}.txt", i / 64),
                std::io::Cursor::new(&body),
            )
            .expect("append");
    }
    builder
        .into_inner()
        .expect("finish the tar")
        .finish()
        .expect("finish the gzip");
    path
}

/// A plain `.tar` holding 500,000 entries.
///
/// Empty files, so this is half a million headers and almost no data: the
/// question here is what half a million *index entries* cost, not what their
/// contents do.
fn half_a_million_entries() -> PathBuf {
    let path = fixture_dir().join("many.tar");
    if path.exists() {
        return path;
    }
    eprintln!("building {} (500,000 entries) ...", path.display());
    let file =
        std::io::BufWriter::with_capacity(1 << 20, std::fs::File::create(&path).expect("create"));
    let mut builder = tar::Builder::new(file);
    for i in 0..500_000u32 {
        let mut header = tar::Header::new_gnu();
        header.set_size(0);
        header.set_mode(0o644);
        header.set_mtime(1_700_000_000);
        header.set_cksum();
        builder
            .append_data(
                &mut header,
                format!("d{:04}/entry-{i:06}.txt", i / 1000),
                std::io::empty(),
            )
            .expect("append");
    }
    builder
        .into_inner()
        .expect("finish")
        .flush()
        .expect("flush");
    path
}

/// Open `container`, time the first row the panel would draw, and watch memory.
async fn measure(tag: &str, container: &Path, dir: &str) {
    let on_disk = std::fs::metadata(container).map(|m| m.len()).unwrap_or(0);
    let before = rss_mib();

    let start = Instant::now();
    let session = session();
    let fs = session.open(&inside(container, "/")).expect("open");

    // Exactly what the panel does: subscribe and draw rows as they arrive.
    let mut rx = fs.read_dir(&inside(container, dir));
    let mut first: Option<std::time::Duration> = None;
    let mut rss_at_first = 0.0;
    let mut rows = 0u64;
    while let Some(item) = rx.recv().await {
        let Ok(entry) = item else { continue };
        if entry.name == ".." {
            continue;
        }
        rows = rows.saturating_add(1);
        if first.is_none() {
            first = Some(start.elapsed());
            rss_at_first = rss_mib();
        }
    }
    let listed = start.elapsed();
    let rss_listed = rss_mib();

    // And the whole index, which is what `Alt+F6` waits for.
    let status = fs.wait_for_index();
    let indexed = start.elapsed();

    println!("\n=== {tag} ===");
    println!(
        "  container on disk       {:>10.1} MiB",
        on_disk as f64 / (1024.0 * 1024.0)
    );
    println!("  RSS before opening      {before:>10.1} MiB");
    println!(
        "  time to first entry     {:>10.1} ms   (RSS {rss_at_first:.1} MiB)",
        first.map_or(f64::NAN, |d| d.as_secs_f64() * 1000.0)
    );
    println!(
        "  time to list {rows:>7} {:>10.1} ms   (RSS {rss_listed:.1} MiB)",
        listed.as_secs_f64() * 1000.0
    );
    println!(
        "  time to full index      {:>10.1} ms   status {status:?}",
        indexed.as_secs_f64() * 1000.0
    );
    println!("  peak RSS (VmHWM)        {:>10.1} MiB", peak_rss_mib());

    assert!(first.is_some(), "{tag}: the panel got no rows at all");
}

/// in the words it uses: a 4 GB `.tar.gz` is listed without
/// being decompressed in full.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "builds a 4 GiB fixture; run with --ignored"]
async fn a_four_gigabyte_targz_streams() {
    let container = four_gigabyte_targz();
    measure("4 GiB .tar.gz", &container, "/d000").await;
}

/// The other end of the same claim: a great many entries rather than a great
/// many bytes.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "builds a 500,000-entry fixture; run with --ignored"]
async fn half_a_million_entries_stream() {
    let container = half_a_million_entries();
    measure("500,000 entries .tar", &container, "/d0000").await;
}
