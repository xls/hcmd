//! the promise, checked by breaking it on purpose.
//!
//! > The rewrite itself writes to a temp file and renames over the original
//! > only on success, so an interrupted or failed rewrite never destroys the
//! > archive. The original is unlinked only after the rename succeeds.
//!
//! Three ways to interrupt one, because they fail differently:
//!
//! * a **cancel**, where the code is in control and unwinds normally;
//! * an **error mid-write**, where a source disappears after bytes have
//!   already gone into the temp file;
//! * a **`SIGKILL`**, where no destructor runs at all, no buffer is flushed,
//!   and the only thing standing between the user and a lost archive is the
//!   fact that the original was never opened for writing.
//!
//! The last one needs a real process to kill, so the test re-invokes its own
//! binary in a child mode and shoots it part way through.
//!
//! ```text
//! cargo test --release --test archive_durability -- --ignored --nocapture
//! ```

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

use holoscommander::vfs::archive::format::{FormatId, MemberEdit, NoProgress, WriteProgress};
use holoscommander::vfs::archive::index::{IndexSink, RawMember};

/// The environment variable that puts a re-invoked test binary into child mode.
const CHILD: &str = "HCMD_DURABILITY_CHILD";

fn scratch(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "hcmd-durability-{tag}-{}-{nanos:x}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

/// A byte pattern that compresses but is not all one value, so the rewrite is
/// real work rather than a run-length trick.
fn filler(len: usize, seed: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    let mut n = seed;
    while out.len() < len {
        let _ = writeln!(
            out,
            "member {seed} line {n} with enough text to be worth compressing"
        );
        n = n.wrapping_add(1);
    }
    out.truncate(len);
    out
}

/// A `.tar.gz` of `members` members, each `each` bytes.
fn build_targz(path: &Path, members: usize, each: usize) {
    let file = std::fs::File::create(path).expect("create");
    let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
    let mut builder = tar::Builder::new(encoder);
    for i in 0..members {
        let body = filler(each, i as u64);
        let mut header = tar::Header::new_gnu();
        header.set_size(body.len() as u64);
        header.set_mode(0o644);
        header.set_mtime(1_700_000_000);
        header.set_cksum();
        builder
            .append_data(
                &mut header,
                format!("payload/member-{i:03}.txt"),
                std::io::Cursor::new(body),
            )
            .expect("append");
    }
    builder
        .into_inner()
        .expect("finish tar")
        .finish()
        .expect("finish gzip");
}

/// A cheap content fingerprint. Not cryptographic; it only has to notice that
/// a file changed.
fn fingerprint(path: &Path) -> (u64, u64) {
    let bytes = std::fs::read(path).expect("read the archive");
    let mut hash = 0xcbf29ce484222325u64;
    for b in &bytes {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    (bytes.len() as u64, hash)
}

/// The archive still opens and still holds the members it held.
///
/// Byte-for-byte equality says the file did not change; this says the file is
/// still an archive, which is the thing the user actually kept.
fn still_a_working_archive(path: &Path, expect: usize) {
    #[derive(Default)]
    struct Count(usize);
    impl IndexSink for Count {
        fn push(&mut self, _raw: RawMember) -> bool {
            self.0 = self.0.saturating_add(1);
            true
        }
        fn cancelled(&self) -> bool {
            false
        }
    }
    let mut sink = Count::default();
    FormatId::TarGz
        .backend()
        .index(path, &mut sink)
        .expect("the original still indexes");
    assert!(
        sink.0 >= expect,
        "the original lost members: {} < {expect}",
        sink.0
    );
}

/// No `.hcmd-rewrite-*` file left beside the archive.
fn leftovers(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .map(|n| n.to_string_lossy().contains("hcmd-rewrite"))
                .unwrap_or(false)
        })
        .collect()
}

/// A `WriteProgress` that cancels once `after` bytes have gone by.
struct CancelAfter {
    after: u64,
    seen: u64,
}

impl WriteProgress for CancelAfter {
    fn bytes(&mut self, n: u64) -> bool {
        self.seen = self.seen.saturating_add(n);
        self.seen < self.after
    }
}

/// Cancelling a compressed-tar rewrite leaves the original untouched.
#[test]
fn a_cancelled_rewrite_leaves_the_archive_alone() {
    let dir = scratch("cancel");
    let container = dir.join("archive.tar.gz");
    build_targz(&container, 40, 256 * 1024);
    let before = fingerprint(&container);

    let source = dir.join("addition.txt");
    std::fs::write(&source, filler(64 * 1024, 999)).expect("source");

    let edits = vec![MemberEdit::Put {
        member_path: "payload/added.txt".to_string(),
        source,
        mode: 0o644,
        mtime: None,
    }];
    let mut progress = CancelAfter {
        after: 128 * 1024,
        seen: 0,
    };
    let outcome = FormatId::TarGz
        .backend()
        .apply(&container, &edits, &mut progress);

    assert!(outcome.is_err(), "a cancel is not a success");
    assert_eq!(fingerprint(&container), before, "the original changed");
    still_a_working_archive(&container, 40);
    assert!(
        leftovers(&dir).is_empty(),
        "a cancelled rewrite left {:?}",
        leftovers(&dir)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A rewrite that fails part way through leaves the original untouched.
///
/// The failure is a source file that is not there: the rewrite copies the
/// original's members into the temp file, reaches this one, and cannot go on.
#[test]
fn a_failed_rewrite_leaves_the_archive_alone() {
    let dir = scratch("error");
    let container = dir.join("archive.tar.gz");
    build_targz(&container, 40, 256 * 1024);
    let before = fingerprint(&container);

    let real = dir.join("real.txt");
    std::fs::write(&real, filler(256 * 1024, 7)).expect("source");

    let edits = vec![
        MemberEdit::Put {
            member_path: "payload/first-added.txt".to_string(),
            source: real,
            mode: 0o644,
            mtime: None,
        },
        MemberEdit::Put {
            member_path: "payload/doomed.txt".to_string(),
            source: dir.join("this-file-does-not-exist"),
            mode: 0o644,
            mtime: None,
        },
    ];
    let outcome = FormatId::TarGz
        .backend()
        .apply(&container, &edits, &mut NoProgress);

    assert!(outcome.is_err(), "a missing source is not a success");
    assert_eq!(fingerprint(&container), before, "the original changed");
    still_a_working_archive(&container, 40);
    assert!(
        leftovers(&dir).is_empty(),
        "a failed rewrite left {:?}",
        leftovers(&dir)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The child half of the `SIGKILL` test: rewrite a large archive, and be shot.
///
/// `#[ignore]` so it never runs as part of the suite, and gated on the
/// environment variable so an explicit `--ignored` run does not start it
/// either.
#[test]
#[ignore = "the child half of the SIGKILL test"]
fn rewrite_child() {
    let Ok(container) = std::env::var(CHILD) else {
        return;
    };
    let container = PathBuf::from(container);
    let dir = container.parent().unwrap_or(Path::new(".")).to_path_buf();
    let source = dir.join("addition.bin");
    let edits = vec![MemberEdit::Put {
        member_path: "payload/added.txt".to_string(),
        source,
        mode: 0o644,
        mtime: None,
    }];
    // Loop, so the parent is certain to catch it mid-rewrite whenever it
    // happens to fire.
    loop {
        let _ = FormatId::TarGz
            .backend()
            .apply(&container, &edits, &mut NoProgress);
    }
}

/// `SIGKILL` part way through a rewrite: no destructor runs, and the archive
/// must still be there.
#[test]
#[ignore = "spawns and kills a child process; run with --ignored"]
fn a_sigkilled_rewrite_leaves_the_archive_alone() {
    let dir = scratch("sigkill");
    let container = dir.join("archive.tar.gz");
    // Big enough that a rewrite takes long enough to interrupt.
    build_targz(&container, 200, 1024 * 1024);
    std::fs::write(dir.join("addition.bin"), filler(1024 * 1024, 3)).expect("source");
    let before = fingerprint(&container);
    println!("  original {} bytes, fingerprint {:x}", before.0, before.1);

    let exe = std::env::current_exe().expect("the test binary");
    let mut child = std::process::Command::new(exe)
        .args(["--exact", "rewrite_child", "--ignored", "--nocapture"])
        .env(CHILD, &container)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn the child");

    // Wait until the child is demonstrably mid-rewrite: a temp file exists
    // beside the archive and has grown past a megabyte.
    let start = std::time::Instant::now();
    let mut caught = false;
    while start.elapsed() < std::time::Duration::from_secs(60) {
        let big = leftovers(&dir)
            .iter()
            .filter_map(|p| std::fs::metadata(p).ok())
            .any(|m| m.len() > 1024 * 1024);
        if big {
            caught = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(caught, "never caught the child mid-rewrite");
    println!(
        "  killing the child mid-rewrite after {:?}",
        start.elapsed()
    );

    // SIGKILL: no unwinding, no `Drop`, no flush.
    unsafe_kill(&child);
    let _ = child.wait();

    let after = fingerprint(&container);
    println!("  after    {} bytes, fingerprint {:x}", after.0, after.1);
    assert_eq!(after, before, "SIGKILL destroyed the archive");
    still_a_working_archive(&container, 200);

    let orphans = leftovers(&dir);
    println!(
        "  temp files the killed child left behind: {}",
        orphans.len()
    );
    for path in &orphans {
        let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        println!("    {} ({size} bytes)", path.display());
    }
    assert!(
        !orphans.is_empty(),
        "expected a SIGKILL to leave a temp file, or this test proves nothing"
    );

    // Nothing else will ever remove those: step 10's startup
    // sweep covers the session cache, and this file is beside the user's own
    // archive. The next rewrite of this archive is what clears them.
    let edits = vec![MemberEdit::Put {
        member_path: "payload/added.txt".to_string(),
        source: dir.join("addition.bin"),
        mode: 0o644,
        mtime: None,
    }];
    FormatId::TarGz
        .backend()
        .apply(&container, &edits, &mut NoProgress)
        .expect("a rewrite after the crash still works");
    let after_sweep = leftovers(&dir);
    println!(
        "  after the next rewrite:                 {}",
        after_sweep.len()
    );
    assert!(
        after_sweep.is_empty(),
        "the dead session's litter survived: {after_sweep:?}"
    );
    still_a_working_archive(&container, 201);

    let _ = std::fs::remove_dir_all(&dir);
}

/// `SIGKILL` without pulling in a libc dependency: the design fixes the
/// crate list, and `Child::kill` is `SIGKILL` on Unix already.
fn unsafe_kill(child: &std::process::Child) {
    let pid = child.id();
    let _ = std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status();
}
