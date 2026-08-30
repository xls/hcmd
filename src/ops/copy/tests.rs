//! Tests for the copy engine, and the fixtures its siblings share.
//!
//! In a companion file rather than inline, which is the pattern the
//! viewer, the app and the archive backend already use: it keeps the
//! production file to the code that ships. `conflict` and `move_` import
//! `TempTree`, `drive` and the spec builders from here, which is why the
//! module is `pub(crate)` rather than private.

use super::*;
use crate::ops::{CancelFlag, ConflictChoice, JobKind, JobSpec};
use crate::remote::transport::RemoteTransport;
use crate::vfs::LocalFs;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A directory under `$TMPDIR` that removes itself.
///
/// `pub(crate)` so `conflict`, `move_` and `queue` share one implementation
/// rather than growing three subtly different ones.
pub(crate) struct TempTree(PathBuf);

impl TempTree {
    pub(crate) fn new(tag: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_nanos();
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "hcmd-copy-{tag}-{pid}-{nanos}-{n}",
            pid = std::process::id()
        ));
        fs::create_dir_all(&root).expect("temp tree");
        Self(root)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }

    pub(crate) fn file(&self, rel: &str, contents: &[u8]) -> PathBuf {
        let p = self.0.join(rel);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).expect("parents");
        }
        fs::write(&p, contents).expect("write");
        p
    }

    pub(crate) fn dir(&self, rel: &str) -> PathBuf {
        let p = self.0.join(rel);
        fs::create_dir_all(&p).expect("mkdir");
        p
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A router with one live connection over an in-memory transport.
///
/// The **router**, not the backend, because that is what a job is really
/// handed: `ops::spawn` gets `App::vfs`, which routes each path to the
/// backend that owns it. Handing a job one backend directly
/// would test a shape the program never has.
fn remote_router(
    transport: crate::remote::transport::FakeTransport,
) -> (
    Arc<crate::remote::transport::FakeTransport>,
    Arc<crate::vfs::VfsRouter>,
    crate::remote::RemoteId,
) {
    let transport = Arc::new(transport);
    let fs = crate::remote::RemoteFs::new(
        crate::remote::Target {
            protocol: crate::remote::Protocol::Sftp,
            host: "nas.local".to_string(),
            port: 22,
            user: "thorin".to_string(),
            dir: None,
        },
        Arc::clone(&transport) as Arc<dyn crate::remote::transport::RemoteTransport>,
        Duration::from_secs(2),
    );
    let router = Arc::new(crate::vfs::VfsRouter::new(
        crate::config::ArchiveConfig::default(),
        crate::config::RemoteConfig::default(),
    ));
    let id = router.remotes().register(fs).expect("register");
    (transport, router, id)
}

/// I14: `Error::ConnectionLost` **stops a batch** rather than failing every
/// remaining file with the same message.
///
/// Four files, the connection dropped on the third: one failure, and the
/// fourth is never attempted. Two hundred identical rows in a failure
/// summary say less than one.
#[test]
fn a_lost_connection_stops_the_batch_instead_of_failing_every_file() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("a runtime");
    let _guard = runtime.enter();

    let tree = TempTree::new("lost");
    let dest = tree.dir("into");
    let (transport, router, id) = remote_router(
        crate::remote::transport::FakeTransport::new()
            .with_dir("/srv")
            .with_file("/srv/a.txt", b"aaa")
            .with_file("/srv/b.txt", b"bbb")
            .with_file("/srv/c.txt", b"ccc")
            .with_file("/srv/d.txt", b"ddd")
            .drop_connection_at("/srv/c.txt"),
    );
    let sources: Vec<VfsPath> = ["a", "b", "c", "d"]
        .iter()
        .map(|n| id.path(&format!("/srv/{n}.txt")))
        .collect();
    let spec = JobSpec::new(JobKind::Copy, sources, Some(VfsPath::local(&dest)));
    let (mut ctx, rx, _dtx, _flag) = JobContext::for_test(spec.kind);
    run(router.as_ref(), &spec, &mut ctx);
    let summary = ctx.finish();
    drop(rx);

    assert_eq!(
        summary.failures.len(),
        1,
        "one failure, not one per remaining file: {:?}",
        summary.failures
    );
    assert!(dest.join("a.txt").exists());
    assert!(dest.join("b.txt").exists());
    assert!(
        !dest.join("d.txt").exists(),
        "the fourth file was attempted after the connection had gone"
    );
    assert!(!transport.is_live(), "the transport knows it has gone");
}

/// a recursive download cannot be steered out of the
/// destination by a name the *server* chose (Zip Slip's remote spelling).
///
/// `copy_tree_via_vfs` builds every child's destination as
/// `dst.join(&child.name)`, and `Path::join` neither rejects `..` nor
/// refuses an absolute argument - `~/dl`.join("/etc/cron.d/pwn") is
/// `/etc/cron.d/pwn`. A hostile server therefore aims at the listing, not
/// at the transfer. Two spellings, both refused, and the transport serves
/// the bytes so that the write would have succeeded had they not been.
#[test]
fn a_server_chosen_name_cannot_write_outside_the_destination() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("a runtime");
    let _guard = runtime.enter();

    let tree = TempTree::new("zipslip");
    let dest = tree.dir("into");
    let (_transport, router, id) = remote_router(
        crate::remote::transport::FakeTransport::new()
            .with_dir("/srv")
            .with_dir("/srv/share")
            .with_file("/srv/share/ok.txt", b"fine")
            .with_listing_name("/srv/share", "../../escaped.txt", b"PWNED")
            .with_listing_name("/srv/share", "/etc/cron.d/pwn", b"PWNED"),
    );
    let spec = JobSpec::new(
        JobKind::Copy,
        vec![id.path("/srv/share")],
        Some(VfsPath::local(&dest)),
    );
    let (mut ctx, rx, _dtx, _flag) = JobContext::for_test(spec.kind);
    run(router.as_ref(), &spec, &mut ctx);
    let summary = ctx.finish();
    drop(rx);

    assert!(
        dest.join("share/ok.txt").exists(),
        "the honest file was still copied"
    );
    assert!(
        !tree.path().join("escaped.txt").exists(),
        "a `..` name walked out of the destination"
    );
    assert!(
        !dest.join("escaped.txt").exists(),
        "the `..` name was neither followed nor flattened into the destination"
    );
    let outside = fs::read_dir(tree.path())
        .expect("the temp root")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert_eq!(outside, vec!["into".to_string()], "{outside:?}");
    assert_eq!(summary.files_done, 1, "one file, and only the honest one");
}

/// The same rule on the side that does the joining, so no future backend
/// can reintroduce it by forgetting to filter.
///
/// `list_via_vfs` drops the row **and says so**: a listing that was edited
/// on the way in is a failure of that directory, which is what stops a
/// move from deleting the source.
#[test]
fn a_listing_row_that_is_a_path_is_refused_by_the_walk_itself() {
    /// A backend that answers one directory with three rows, two of which
    /// no well-behaved backend would send.
    struct HostileListing;

    impl Vfs for HostileListing {
        fn kind(&self) -> crate::vfs::BackendKind {
            crate::vfs::BackendKind::Local
        }

        fn read_dir(&self, _path: &VfsPath) -> tokio::sync::mpsc::Receiver<Result<Entry>> {
            let (tx, rx) = tokio::sync::mpsc::channel(8);
            let mut parent = Entry::dir("..");
            parent.is_parent = true;
            for entry in [
                parent,
                Entry::file("ok.txt"),
                Entry::file("../../.bashrc"),
                Entry::file("/etc/cron.d/pwn"),
            ] {
                let _ = tx.blocking_send(Ok(entry));
            }
            rx
        }

        fn stat(&self, _path: &VfsPath) -> Result<Entry> {
            Err(Error::NotFound("nothing is read here".to_string()))
        }

        fn open_read(&self, _path: &VfsPath) -> Result<Box<dyn std::io::Read + Send>> {
            Err(Error::Unsupported("read"))
        }

        fn open_write(&self, _path: &VfsPath) -> Result<Box<dyn std::io::Write + Send>> {
            Err(Error::Unsupported("write"))
        }

        fn create_dir(&self, _path: &VfsPath) -> Result<()> {
            Err(Error::Unsupported("mkdir"))
        }

        fn remove(&self, _path: &VfsPath) -> Result<()> {
            Err(Error::Unsupported("remove"))
        }

        fn rename(&self, _from: &VfsPath, _to: &VfsPath) -> Result<()> {
            Err(Error::Unsupported("rename"))
        }

        fn capabilities(&self) -> crate::vfs::Capabilities {
            crate::vfs::Capabilities::LOCAL
        }
    }

    let (rows, failure) = vfs::list_via_vfs(&HostileListing, &VfsPath::local("/anywhere"));
    let names: Vec<&str> = rows.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, vec!["ok.txt"], "only plain names are walked");
    let told = failure.map(|err| err.to_string()).unwrap_or_default();
    assert!(told.contains("2 entries were refused"), "{told}");
    assert!(
        !told.contains(".bashrc"),
        "the name the other side chose is not quoted back: {told}"
    );
}

/// I14's other half: a connection **closed** under a running batch stops
/// it too.
///
/// `Ctrl+F` on the panel, a tab closed, a panel navigated away - the
/// registry forgets the id and every later lookup answers "that connection
/// has been closed". That answer used to be an `Error::Msg`, which
/// `is_fatal` maps to false, so a twenty-file batch produced twenty
/// identical failure rows instead of one. The sibling test above passes on
/// `Error::ConnectionLost`; this is the same event told from this end.
#[test]
fn a_connection_closed_under_the_batch_stops_it_too() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("a runtime");
    let _guard = runtime.enter();

    let tree = TempTree::new("closed");
    let dest = tree.dir("into");
    let mut transport = crate::remote::transport::FakeTransport::new().with_dir("/srv");
    for n in 0..20 {
        transport = transport.with_file(&format!("/srv/{n}.txt"), b"bytes");
    }
    let (_transport, router, id) = remote_router(transport);
    let sources: Vec<VfsPath> = (0..20).map(|n| id.path(&format!("/srv/{n}.txt"))).collect();
    let spec = JobSpec::new(JobKind::Copy, sources, Some(VfsPath::local(&dest)));
    let (mut ctx, rx, _dtx, _flag) = JobContext::for_test(spec.kind);

    // Exactly what the panel does: the id is forgotten, the job runs on.
    router.remotes().close(id);
    run(router.as_ref(), &spec, &mut ctx);
    let summary = ctx.finish();
    drop(rx);

    assert_eq!(
        summary.failures.len(),
        1,
        "one row, not one per remaining file: {:?}",
        summary.failures
    );
    let told = summary
        .failures
        .first()
        .map(|f| f.error.clone())
        .unwrap_or_default();
    assert!(
        told.contains("that connection has been closed"),
        "the contract's wording survives the variant: {told}"
    );
}

/// I12: `ops::chunk_size` is what sizes the copy buffer, so a
/// `LatencyClass::Network` backend copies in larger chunks than a local one.
///
///
/// Asserted where it can actually be observed: the length of the buffer the
/// copy loop hands to `Read::read` on the remote side.
#[test]
fn a_network_backend_is_read_in_network_sized_chunks() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("a runtime");
    let _guard = runtime.enter();

    let tree = TempTree::new("chunk");
    let dest = tree.dir("into");
    let big = vec![7u8; COPY_CHUNK * 2];
    let (transport, router, id) = remote_router(
        crate::remote::transport::FakeTransport::new()
            .with_dir("/srv")
            .with_file("/srv/big.bin", &big),
    );
    let spec = JobSpec::new(
        JobKind::Copy,
        vec![id.path("/srv/big.bin")],
        Some(VfsPath::local(&dest)),
    );
    let (mut ctx, rx, _dtx, _flag) = JobContext::for_test(spec.kind);
    run(router.as_ref(), &spec, &mut ctx);
    drop(rx);

    let network = crate::ops::chunk_size(&crate::vfs::Capabilities::SFTP);
    assert!(
        network > crate::ops::chunk_size(&crate::vfs::Capabilities::LOCAL),
        "a network backend copies in larger chunks than a local one"
    );
    assert_eq!(
        transport.max_read(),
        network,
        "the copy loop sized its buffer from Capabilities, not from a constant"
    );
    assert_eq!(
        std::fs::read(dest.join("big.bin")).expect("the copy"),
        big,
        "and it copied the bytes"
    );
}

/// Drive a runner synchronously; the receiver is kept alive so nothing is
/// cancelled by a dropped channel.
pub(crate) fn drive(spec: JobSpec) -> crate::ops::JobSummary {
    let (mut ctx, rx, _dtx, _flag) = JobContext::for_test(spec.kind);
    let fs_impl = LocalFs::new();
    crate::ops::run(&fs_impl, &spec, &mut ctx);
    let summary = ctx.finish();
    drop(rx);
    summary
}

/// Drive a runner with decisions already queued, so a conflict is answered
/// without a UI. Extra answers left over prove nothing asked for them.
pub(crate) fn drive_answering(
    spec: JobSpec,
    answers: Vec<crate::ops::Decision>,
) -> crate::ops::JobSummary {
    let (mut ctx, rx, dtx, _flag) = JobContext::for_test(spec.kind);
    for answer in answers {
        dtx.try_send(answer).expect("queue an answer");
    }
    let fs_impl = LocalFs::new();
    crate::ops::run(&fs_impl, &spec, &mut ctx);
    let summary = ctx.finish();
    drop(rx);
    summary
}

pub(crate) fn copy_spec(sources: Vec<PathBuf>, dest: &Path) -> JobSpec {
    JobSpec::new(
        JobKind::Copy,
        sources.into_iter().map(VfsPath::local).collect(),
        Some(VfsPath::local(dest)),
    )
}

pub(crate) fn move_spec(sources: Vec<PathBuf>, dest: &Path) -> JobSpec {
    JobSpec::new(
        JobKind::Move,
        sources.into_iter().map(VfsPath::local).collect(),
        Some(VfsPath::local(dest)),
    )
}

/// the terminal is restored on every exit path - and this is
/// the one a file manager can get wrong.
///
/// `main::run` drops the tokio runtime after `event_loop` returns, and a
/// job runs on the **blocking** pool ([`crate::ops::spawn`]), so dropping
/// the runtime waits for it. If a copy in flight did not notice that the
/// UI had gone, quitting mid-transfer would restore the terminal and then
/// sit there, apparently wedged, until the copy finished.
///
/// It notices because the progress channel is how it reports: a send to a
/// dropped receiver sets `lost`, which is [`JobContext::cancelled`], which
/// is what the copy loop checks between files and inside its chunk loop.
/// The event loop drops that receiver on the way out, so this test drops it
/// the same way and asserts the worker stopped rather than ran to
/// completion.
#[test]
fn a_copy_in_flight_does_not_hold_the_runtime_open() {
    let tree = TempTree::new("teardown");
    let src = tree.dir("src");
    // Enough files that the copy cannot possibly finish inside the window
    // between the first event and the drop below, and small enough that
    // building the tree costs nothing.
    const FILES: usize = 3000;
    for n in 0..FILES {
        tree.file(&format!("src/f{n:04}.bin"), &[b'x'; 512]);
    }
    let dest = tree.dir("dest");

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let (tx, mut rx) = tokio::sync::mpsc::channel(crate::ops::JOB_CHANNEL_DEPTH);
    let spec = copy_spec(vec![src.clone()], &dest);
    // `spawn` puts the job on the blocking pool, which needs the runtime in
    // scope - the event loop is always inside `block_on` when it calls it.
    let guard = runtime.enter();
    let _handle = crate::ops::spawn(
        std::sync::Arc::new(LocalFs::new()),
        crate::ops::JobId(1),
        spec,
        tx,
        &crate::config::OpsConfig::default(),
    );

    // Wait until the worker is genuinely running, then take the UI away
    // exactly as `event_loop` returning does.
    let first = runtime.block_on(rx.recv());
    assert!(first.is_some(), "the worker should have reported");
    drop(rx);

    let started = std::time::Instant::now();
    drop(guard);
    drop(runtime);
    let waited = started.elapsed();
    assert!(
        waited < Duration::from_secs(10),
        "dropping the runtime waited {waited:?} on a copy that should have \
         stopped itself"
    );

    // And it really did stop rather than merely being quick: a full run
    // would have left every file behind.
    let copied = fs::read_dir(dest.join("src"))
        .map(|d| d.flatten().count())
        .unwrap_or(0);
    assert!(
        copied < FILES,
        "the copy ran to completion ({copied} files); the cancellation path \
         is what is under test"
    );
}

/// Names of everything directly inside `dir`, sorted.
pub(crate) fn listing(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .expect("read dir")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn a_tree_is_copied_with_its_structure() {
    let t = TempTree::new("tree");
    t.file("src/a.txt", b"alpha");
    t.file("src/sub/b.txt", b"beta");
    let dest = t.dir("dest");

    let summary = drive(copy_spec(vec![t.path().join("src")], &dest));
    assert!(summary.is_clean(), "{:?}", summary.failures);
    assert_eq!(fs::read(dest.join("src/a.txt")).expect("a"), b"alpha");
    assert_eq!(fs::read(dest.join("src/sub/b.txt")).expect("b"), b"beta");
    assert_eq!(summary.files_done, 2);
    assert_eq!(summary.bytes_done, 9);
}

#[test]
fn a_cancelled_copy_leaves_no_half_written_destination() {
    let t = TempTree::new("cancel");
    let big = vec![b'z'; COPY_CHUNK * 4];
    t.file("big.bin", &big);
    let dest = t.dir("dest");

    let spec = copy_spec(vec![t.path().join("big.bin")], &dest);
    let (mut ctx, rx, _dtx, flag) = JobContext::for_test(JobKind::Copy);
    // Cancel before a single chunk is written.
    flag.cancel();
    crate::ops::run(&LocalFs::new(), &spec, &mut ctx);
    let summary = ctx.finish();
    drop(rx);

    assert!(summary.cancelled);
    assert!(!dest.join("big.bin").exists(), "no destination was left");
    assert!(listing(&dest).is_empty(), "no partial file either");
}

/// A reader that cancels the job as soon as it has produced one chunk.
///
/// This is how the "within a large file's chunk loop" is
/// proved deterministically: racing a thread against a real file would
/// pass on a slow machine and pass vacuously on a fast one.
struct CancelAfterFirstChunk<'a> {
    data: &'a [u8],
    pos: usize,
    flag: CancelFlag,
}

impl Read for CancelAfterFirstChunk<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let left = self.data.get(self.pos..).unwrap_or(&[]);
        let n = left.len().min(buf.len());
        if let (Some(dst), Some(src)) = (buf.get_mut(..n), left.get(..n)) {
            dst.copy_from_slice(src);
        }
        self.pos = self.pos.saturating_add(n);
        // The job is cancelled *after* the bytes are handed over, so the
        // loop has real work in flight when it notices.
        self.flag.cancel();
        Ok(n)
    }
}

#[test]
fn cancelling_inside_the_chunk_loop_leaves_nothing_behind() {
    let t = TempTree::new("midfile");
    let data = vec![b'q'; COPY_CHUNK * 4];
    let src = t.file("big.bin", &data);
    let dest = t.dir("dest");
    let dst = dest.join("big.bin");

    let (mut ctx, rx, _dtx, flag) = JobContext::for_test(JobKind::Copy);
    let meta = fs::metadata(&src).expect("meta");
    let mut reader = CancelAfterFirstChunk {
        data: &data,
        pos: 0,
        flag,
    };
    let outcome = copy_regular_from(
        &mut reader,
        &meta,
        &src,
        &dst,
        &JobOptions::default(),
        &mut ctx,
    );
    drop(rx);

    assert!(
        matches!(outcome, Err(Error::Cancelled)),
        "the chunk loop stopped: {outcome:?}"
    );
    assert!(reader.pos < data.len(), "it stopped part-way through");
    assert!(!dst.exists(), "no destination was left");
    assert!(listing(&dest).is_empty(), "and no partial file either");
}

#[test]
fn a_symlink_is_copied_as_a_link() {
    let t = TempTree::new("symlink");
    t.file("real.txt", b"content");
    std::os::unix::fs::symlink("real.txt", t.path().join("link.txt")).expect("symlink");
    let dest = t.dir("dest");

    let summary = drive(copy_spec(vec![t.path().join("link.txt")], &dest));
    assert!(summary.is_clean(), "{:?}", summary.failures);
    let copied = dest.join("link.txt");
    assert!(
        fs::symlink_metadata(&copied)
            .expect("meta")
            .file_type()
            .is_symlink(),
        "it is still a link"
    );
    assert_eq!(
        fs::read_link(&copied).expect("target"),
        Path::new("real.txt")
    );
}

#[test]
fn a_symlink_loop_does_not_hang_the_job() {
    let t = TempTree::new("loop");
    t.file("tree/inner/a.txt", b"payload");
    // `up` resolves to `tree`, which the copy is already standing in.
    std::os::unix::fs::symlink("../..", t.path().join("tree/inner/up")).expect("symlink");
    let dest = t.dir("dest");

    let mut spec = copy_spec(vec![t.path().join("tree")], &dest);
    spec.options.follow_symlinks = true;
    // If cycle protection were missing this would not return at all.
    let summary = drive(spec);

    assert!(summary.failures.is_empty(), "{:?}", summary.failures);
    assert_eq!(
        fs::read(dest.join("tree/inner/a.txt")).expect("a"),
        b"payload"
    );
    assert!(summary.skipped >= 1, "the cycle was counted, not followed");
}

#[test]
fn preserving_attributes_carries_mode_and_mtime() {
    let t = TempTree::new("preserve");
    let src = t.file("a.sh", b"#!/bin/sh\n");
    fs::set_permissions(&src, fs::Permissions::from_mode(0o750)).expect("chmod");
    let want_mtime = fs::metadata(&src).expect("meta").modified().expect("mtime");
    let dest = t.dir("dest");

    let mut spec = copy_spec(vec![src.clone()], &dest);
    spec.options.preserve_attrs = true;
    assert!(drive(spec).is_clean());

    let got = fs::metadata(dest.join("a.sh")).expect("meta");
    assert_eq!(got.mode() & 0o777, 0o750);
    assert_eq!(got.modified().expect("mtime"), want_mtime);
}

#[test]
fn what_is_preserved_is_stated_rather_than_implied() {
    // the checkbox must not promise uid/gid or
    // xattrs, because std cannot reach either.
    assert_eq!(PRESERVED, &["mode", "mtime", "atime"]);
    assert!(NOT_PRESERVED.contains("uid/gid"));
    assert!(NOT_PRESERVED.contains("xattr"));
}

#[test]
fn a_run_of_zeros_becomes_a_hole_rather_than_written_bytes() {
    let t = TempTree::new("sparse");
    // One dense chunk, then four chunks of zeros.
    let mut content = vec![b'x'; COPY_CHUNK];
    content.extend(std::iter::repeat_n(0u8, COPY_CHUNK * 4));
    let src = t.file("sparse.bin", &content);
    let dest = t.dir("dest");

    assert!(drive(copy_spec(vec![src], &dest)).is_clean());
    let out = dest.join("sparse.bin");
    let meta = fs::metadata(&out).expect("meta");
    assert_eq!(meta.len() as usize, content.len(), "the length is right");
    assert_eq!(
        fs::read(&out).expect("read"),
        content,
        "and so are the bytes"
    );
    // `blocks()` is in 512-byte units. A dense copy would need at least
    // len/512 of them; a sparse one needs far fewer. Filesystems differ, so
    // this asserts the direction rather than an exact figure - and only
    // where the filesystem under `$TMPDIR` can hold a hole at all, which a
    // probe answers rather than an assumption about `/tmp`.
    if supports_holes(t.path()) {
        let dense_blocks = (content.len() / 512) as u64;
        assert!(
            meta.blocks() < dense_blocks,
            "the copy kept a hole: {} blocks for {} bytes",
            meta.blocks(),
            content.len()
        );
    }
}

/// Does a hole made *the way [`copy_stream`] makes one* survive on this
/// filesystem?
///
/// The probe has to be the technique, not merely the capability. An earlier
/// version asked whether `ftruncate` on an empty file produced a sparse one,
/// which APFS answers yes to while still allocating every block when a hole is
/// made by seeking past a run of zeros between real data. So the probe passed
/// on macOS, the assertion ran, and it failed on a filesystem that had done
/// nothing wrong.
///
/// What is asserted is our copy loop. Where the filesystem declines to keep
/// the hole, the bytes and the length are still checked; only the saving is
/// skipped.
fn supports_holes(dir: &Path) -> bool {
    use std::io::{Seek, SeekFrom, Write};

    let probe = dir.join(".hole-probe");
    let Ok(mut file) = fs::File::create(&probe) else {
        return false;
    };
    const GAP: u64 = 4 * 1024 * 1024;
    // Data, a seek over a run of zeros, data: exactly what the copy does.
    let made = file
        .write_all(b"x")
        .and_then(|()| file.seek(SeekFrom::Current(GAP as i64)).map(|_| ()))
        .and_then(|()| file.write_all(b"x"))
        .and_then(|()| file.sync_all())
        .is_ok();
    drop(file);
    let sparse = made
        && fs::metadata(&probe)
            .map(|m| m.blocks() < GAP / 512)
            .unwrap_or(false);
    let _ = fs::remove_file(&probe);
    sparse
}

#[test]
fn a_file_mask_filters_what_is_copied() {
    let t = TempTree::new("mask");
    t.file("src/keep.rs", b"rs");
    t.file("src/drop.md", b"md");
    let dest = t.dir("dest");

    let mut spec = copy_spec(vec![t.path().join("src")], &dest);
    spec.options.file_mask = "*.rs".to_string();
    let summary = drive(spec);

    assert!(summary.is_clean(), "{:?}", summary.failures);
    assert!(dest.join("src/keep.rs").exists());
    assert!(!dest.join("src/drop.md").exists());
    assert_eq!(summary.skipped, 1);
}

#[test]
fn a_directory_cannot_be_copied_into_itself() {
    let t = TempTree::new("selfcopy");
    let src = t.dir("a");
    let dest = t.dir("a/b");
    let summary = drive(copy_spec(vec![src], &dest));
    assert_eq!(summary.failures.len(), 1);
    assert!(summary.failures[0].error.contains("into itself"));
}

#[test]
fn one_failure_does_not_abort_the_batch() {
    let t = TempTree::new("partial");
    let good = t.file("good.txt", b"ok");
    let missing = t.path().join("missing.txt");
    let dest = t.dir("dest");

    let summary = drive(copy_spec(vec![missing, good], &dest));
    assert_eq!(summary.failures.len(), 1);
    assert_eq!(summary.files_done, 1, "the good file still went across");
    assert!(dest.join("good.txt").exists());
}

#[test]
fn a_failure_deep_in_a_tree_still_lets_the_rest_finish() {
    let t = TempTree::new("deepfail");
    t.file("src/one.txt", b"1");
    t.file("src/locked/two.txt", b"2");
    t.file("src/three.txt", b"3");
    // A directory nobody may read: the walk reports it and carries on.
    fs::set_permissions(
        t.path().join("src/locked"),
        fs::Permissions::from_mode(0o000),
    )
    .expect("chmod");
    let dest = t.dir("dest");

    let summary = drive(copy_spec(vec![t.path().join("src")], &dest));

    // Restore before the drop, or the temp tree cannot be removed.
    let _ = fs::set_permissions(
        t.path().join("src/locked"),
        fs::Permissions::from_mode(0o755),
    );

    // Running as root defeats the permission bits entirely; the assertion
    // that matters either way is that both readable files arrived.
    assert!(dest.join("src/one.txt").exists());
    assert!(dest.join("src/three.txt").exists());
    assert!(summary.files_done >= 2);
}

/// A destination that is not there any more, so both halves of an
/// attribute stamp fail for a reason no test needs privileges to arrange.
///
/// `chmod` on a path that does not exist is `ENOENT`, which is exactly the
/// shape of the `EROFS` and `EPERM` a real mount answers with - the point
/// being what the code does with the error, not which error it is.
fn unstampable(t: &TempTree) -> PathBuf {
    t.path().join("gone/a.txt")
}

#[test]
fn a_mode_that_cannot_be_set_is_a_warning_and_not_a_failed_copy() {
    // The bug: `preserve` did `set_permissions(..)?`, so a filesystem that
    // refuses a `chmod` failed the whole copy - and the caller then
    // deleted a temporary file into which every byte had already been
    // written and flushed. Preservation is an attribute of a finished
    // copy, so it warns and the bytes stay.
    let t = TempTree::new("preserve-warn");
    let src = t.file("a.txt", b"payload");
    let meta = fs::symlink_metadata(&src).expect("stat");
    let handle = fs::File::open(&src).expect("open");

    let warnings = preserve(&meta, &handle, &unstampable(&t));

    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
        warnings
            .first()
            .is_some_and(|w| w.contains("mode") && w.contains("not preserved")),
        "{warnings:?}"
    );
}

#[test]
fn the_local_and_the_extracted_path_report_the_same_refusal() {
    // The asymmetry this pins: copying a file onto a mount that refuses
    // `chmod` threw the copy away, while extracting the same file out of
    // an archive landed it at the umask default and reported success.
    // One policy, one sentence, whichever way the bytes arrived.
    let t = TempTree::new("preserve-agree");
    let src = t.file("a.txt", b"payload");
    let meta = fs::symlink_metadata(&src).expect("stat");
    let handle = fs::File::open(&src).expect("open");
    let dest = unstampable(&t);

    let mut entry = Entry::file("a.txt");
    entry.mode = meta.mode() & 0o7777;

    assert_eq!(
        preserve(&meta, &handle, &dest),
        preserve_entry(&entry, true, &handle, &dest),
    );
    assert!(!preserve_entry(&entry, true, &handle, &dest).is_empty());
}

#[test]
fn a_directory_stamp_that_fails_is_reported_rather_than_swallowed() {
    // The third spelling of the same policy. `stamp_directories` runs
    // after the tree is written, and every one of its calls used to be a
    // `let _ =`: a directory that came out 0755 where the source was 0700
    // was a permission change nobody was told about.
    let t = TempTree::new("stamp-warn");
    let src = t.dir("src");
    let dst = t.path().join("gone/src");

    let (mut ctx, rx, _dtx, _flag) = JobContext::for_test(JobKind::Copy);
    stamp_directories(&[(src, dst)], &mut ctx);
    let summary = ctx.finish();
    drop(rx);

    assert!(
        !summary.failures.is_empty(),
        "the directory stamp failed in silence"
    );
    assert!(
        summary
            .failures
            .iter()
            .any(|f| f.error.contains("not preserved")),
        "{:?}",
        summary.failures
    );
}

#[test]
fn a_copy_that_preserves_everything_it_was_asked_to_warns_about_nothing() {
    // The other side of the policy: a warning that fires on an ordinary
    // copy is noise, and noise is how a real warning gets ignored.
    let t = TempTree::new("preserve-quiet");
    let src = t.file("a.txt", b"payload");
    fs::set_permissions(&src, fs::Permissions::from_mode(0o640)).expect("chmod");
    let dest = t.dir("dest");

    let mut spec = copy_spec(vec![src], &dest);
    spec.options.preserve_attrs = true;
    let summary = drive(spec);

    assert!(summary.is_clean(), "{:?}", summary.failures);
    assert_eq!(
        fs::symlink_metadata(dest.join("a.txt"))
            .expect("stat")
            .mode()
            & 0o7777,
        0o640
    );
}

#[test]
fn a_read_only_destination_is_refused_before_anything_is_read() {
    // "a read-only backend causes `F5` *into* it to be refused
    // up front with a clear message rather than failing halfway through a
    // copy." the design says the same for archives; the rule is about
    // writability, so it is tested against the read-only backend that
    // already exists.
    let t = TempTree::new("readonly");
    let src = t.file("a.txt", b"payload");
    let dest = t.dir("dest");

    let spec = copy_spec(vec![src], &dest);
    let (mut ctx, rx, _dtx, _flag) = JobContext::for_test(JobKind::Copy);
    let fs_impl = crate::vfs::ListFs::new("results", Vec::new());
    assert!(!fs_impl.capabilities().writable, "the premise");
    crate::ops::run(&fs_impl, &spec, &mut ctx);
    let summary = ctx.finish();
    drop(rx);

    assert_eq!(summary.failures.len(), 1, "one refusal, not one per file");
    assert!(
        summary.failures[0].error.contains("read-only"),
        "{}",
        summary.failures[0].error
    );
    assert_eq!(summary.files_done, 0);
    assert!(listing(&dest).is_empty(), "nothing was written");
}

#[test]
fn a_destination_directory_that_cannot_be_written_is_refused_up_front() {
    // The backend is writable - `LocalFs` always is - and the *directory*
    // is not. the "refused up front with a clear message rather
    // than failing halfway through a copy" is about both.
    let t = TempTree::new("nowrite");
    let a = t.file("a.txt", b"one");
    let b = t.file("b.txt", b"two");
    let dest = t.dir("dest");
    fs::set_permissions(&dest, fs::Permissions::from_mode(0o555)).expect("chmod");

    let summary = drive(copy_spec(vec![a, b], &dest));

    let _ = fs::set_permissions(&dest, fs::Permissions::from_mode(0o755));

    // Running as root defeats the permission bits, and then there is
    // nothing to refuse; the premise is checked rather than assumed.
    if summary.files_done == 2 {
        return;
    }
    assert_eq!(
        summary.failures.len(),
        1,
        "one refusal naming the destination, not one failure per source: {:#?}",
        summary.failures
    );
    assert!(
        summary.failures[0].error.contains("cannot be written"),
        "{}",
        summary.failures[0].error
    );
    assert_eq!(summary.files_done, 0, "not a byte was copied");
    assert!(listing(&dest).is_empty(), "and nothing was created");
}

#[test]
fn a_destination_that_is_not_there_is_refused_once() {
    // The same up-front check catches a typo'd target: one clear refusal
    // before the walk, rather than a "No such file or directory" per file.
    let t = TempTree::new("nodest");
    let a = t.file("a.txt", b"one");
    let b = t.file("b.txt", b"two");
    let missing = t.path().join("no/such/dir");

    let summary = drive(copy_spec(vec![a, b], &missing));

    assert_eq!(summary.failures.len(), 1, "{:#?}", summary.failures);
    assert_eq!(summary.files_done, 0);
}

/// rule 1 of this file. A symlink arriving where a
/// **directory** stands used to `remove_dir_all` that directory and then
/// create the link - so an `Overwrite` answered about a file, or the
/// preset `ops.confirm_overwrite = false`, destroyed a whole tree, and a
/// filesystem that cannot hold a symlink left nothing in its place.
#[test]
fn a_symlink_never_removes_a_directory_that_is_in_its_way() {
    let t = TempTree::new("link-over-dir");
    std::os::unix::fs::symlink("/srv/photos/2026", t.path().join("src/latest"))
        .or_else(|_| {
            fs::create_dir_all(t.path().join("src"))?;
            std::os::unix::fs::symlink("/srv/photos/2026", t.path().join("src/latest"))
        })
        .expect("symlink");
    let dest = t.dir("dest");
    // Five photos in the way, under the name the link has.
    for n in 0..5 {
        t.file(&format!("dest/latest/photo{n}.jpg"), b"jpeg");
    }

    let mut spec = copy_spec(vec![t.path().join("src/latest")], &dest);
    spec.options.conflict = Some(ConflictChoice::Overwrite);
    let summary = drive(spec);

    assert_eq!(
        listing(&dest.join("latest")).len(),
        5,
        "the destination directory still holds its photos"
    );
    assert_eq!(summary.failures.len(), 1, "and the refusal is reported");
    assert!(
        summary.failures[0].error.contains("is a directory"),
        "{}",
        summary.failures[0].error
    );
    assert!(listing(&dest).iter().all(|n| !n.contains(PARTIAL_SUFFIX)));
}

/// The other half of the same rule: replacing a *file* with a link still
/// works, and goes through a rename rather than an unlink-then-create.
#[test]
fn a_symlink_replaces_a_file_in_one_step() {
    let t = TempTree::new("link-over-file");
    t.file("src/real.txt", b"payload");
    std::os::unix::fs::symlink("real.txt", t.path().join("src/link.txt")).expect("symlink");
    let dest = t.dir("dest");
    fs::write(dest.join("link.txt"), b"old").expect("seed");

    let mut spec = copy_spec(vec![t.path().join("src/link.txt")], &dest);
    spec.options.conflict = Some(ConflictChoice::Overwrite);
    let summary = drive(spec);

    assert!(summary.is_clean(), "{:?}", summary.failures);
    assert!(
        fs::symlink_metadata(dest.join("link.txt"))
            .expect("meta")
            .file_type()
            .is_symlink()
    );
    assert_eq!(listing(&dest), vec!["link.txt".to_string()], "no leftovers");
}

/// `Append` is the one write that mutates the destination in
/// place, so it is the one that has to look at what it is opening:
/// `O_APPEND` follows a symlink, which writes outside the directory the
/// user chose - and a link pointing back at the source never reaches EOF,
/// so the loop runs until the filesystem is full.
#[test]
fn appending_never_writes_through_a_symlink() {
    let t = TempTree::new("append-link");
    let outside = t.file("outside.conf", b"ORIGINAL-CONF\n");
    let src = t.file("src/report.log", b"PAYLOAD\n");
    let dest = t.dir("dest");
    std::os::unix::fs::symlink(&outside, dest.join("report.log")).expect("symlink");

    let mut spec = copy_spec(vec![src], &dest);
    spec.options.conflict = Some(ConflictChoice::Append);
    let summary = drive(spec);

    assert_eq!(
        fs::read(&outside).expect("read"),
        b"ORIGINAL-CONF\n",
        "the file outside the destination was not touched"
    );
    assert_eq!(summary.failures.len(), 1, "{:?}", summary.failures);
    assert!(
        summary.failures[0].error.contains("symlink"),
        "{}",
        summary.failures[0].error
    );
    assert_eq!(summary.files_done, 0);
}

/// The self-append case, which is the same defect seen from the other end:
/// reader and writer on one inode never reach EOF.
#[test]
fn appending_a_file_to_itself_is_refused_rather_than_unbounded() {
    let t = TempTree::new("append-self");
    let src = t.file("notes.txt", &[b'n'; 4096]);
    let dest = t.dir("dest");
    // A hard link: same inode, different path, so a name comparison misses
    // it and only `(dev, ino)` sees it.
    fs::hard_link(&src, dest.join("notes.txt")).expect("hard link");

    let mut spec = copy_spec(vec![src.clone()], &dest);
    spec.options.conflict = Some(ConflictChoice::Append);
    let summary = drive(spec);

    assert_eq!(
        fs::metadata(&src).expect("meta").len(),
        4096,
        "the source did not grow"
    );
    assert_eq!(summary.failures.len(), 1, "{:?}", summary.failures);
    assert!(
        summary.failures[0].error.contains("same file"),
        "{}",
        summary.failures[0].error
    );
}

/// the rule 1 again, this time against a **second job**: two
/// copies of two different files with the same basename into one directory
/// used to share `.name.hcmd-part`, interleave into one inode, and report a
/// clean copy of a file that is a mixture of both.
#[test]
fn two_jobs_writing_one_destination_name_do_not_share_a_partial_file() {
    let t = TempTree::new("partial-race");
    const SIZE: usize = 4 * COPY_CHUNK;
    let a = t.file("a/movie.mkv", &vec![b'A'; SIZE]);
    let b = t.file("b/movie.mkv", &vec![b'B'; SIZE]);
    let dest = t.dir("dest");

    let dest_a = dest.clone();
    let dest_b = dest.clone();
    let one = std::thread::spawn(move || drive(copy_spec(vec![a], &dest_a)));
    let two = std::thread::spawn(move || drive(copy_spec(vec![b], &dest_b)));
    let first = one.join().expect("job 1");
    let second = two.join().expect("job 2");

    let bytes = fs::read(dest.join("movie.mkv")).expect("read");
    assert_eq!(bytes.len(), SIZE, "a whole file, not a mixture of two");
    let a_bytes = bytes.iter().filter(|b| **b == b'A').count();
    let b_bytes = bytes.iter().filter(|b| **b == b'B').count();
    assert!(
        a_bytes == SIZE || b_bytes == SIZE,
        "one source won outright: {a_bytes} A, {b_bytes} B"
    );
    // Whichever lost is allowed to fail (the rename raced), but neither may
    // report success over the other's bytes.
    let winners = [&first, &second]
        .iter()
        .filter(|summary| summary.is_clean())
        .count();
    assert!(winners >= 1, "at least one job succeeded");
    assert!(
        listing(&dest).iter().all(|n| !n.contains(PARTIAL_SUFFIX)),
        "no partial file was left behind: {:?}",
        listing(&dest)
    );
}

/// the "Preservation: mode, mtime". A directory's mode is
/// stamped **after** its children are written: stamping it on the way in
/// makes a mode-555 source directory refuse its own children, and every
/// file under it is lost from the copy.
#[test]
fn a_read_only_source_directory_still_has_its_contents_copied() {
    let t = TempTree::new("ro-src");
    t.file("ro/a.txt", b"alpha");
    fs::set_permissions(t.path().join("ro"), fs::Permissions::from_mode(0o555)).expect("chmod");
    let dest = t.dir("dest");

    let mut spec = copy_spec(vec![t.path().join("ro")], &dest);
    spec.options.preserve_attrs = true;
    let summary = drive(spec);

    let _ = fs::set_permissions(t.path().join("ro"), fs::Permissions::from_mode(0o755));
    let _ = fs::set_permissions(dest.join("ro"), fs::Permissions::from_mode(0o755));

    assert!(summary.is_clean(), "{:?}", summary.failures);
    assert_eq!(fs::read(dest.join("ro/a.txt")).expect("a"), b"alpha");
    assert_eq!(summary.files_done, 1);
}

/// And the mode still arrives - the second pass is a deferral, not a
/// dropped feature.
#[test]
fn a_copied_directory_keeps_its_mode_and_its_timestamps() {
    let t = TempTree::new("dir-attrs");
    t.file("tree/inner/a.txt", b"x");
    let when = std::time::UNIX_EPOCH + Duration::from_secs(1_565_000_000);
    for rel in ["tree/inner", "tree"] {
        let dir = fs::File::open(t.path().join(rel)).expect("open dir");
        dir.set_times(fs::FileTimes::new().set_modified(when).set_accessed(when))
            .expect("stamp");
    }
    fs::set_permissions(t.path().join("tree"), fs::Permissions::from_mode(0o750)).expect("chmod");
    let dest = t.dir("dest");

    let mut spec = copy_spec(vec![t.path().join("tree")], &dest);
    spec.options.preserve_attrs = true;
    assert!(drive(spec).is_clean());

    let copied = fs::metadata(dest.join("tree")).expect("meta");
    assert_eq!(copied.mode() & 0o777, 0o750, "the mode came across");
    assert_eq!(
        copied.modified().expect("mtime"),
        when,
        "and so did the mtime, after the children were written"
    );
    assert_eq!(
        fs::metadata(dest.join("tree/inner"))
            .expect("meta")
            .modified()
            .expect("mtime"),
        when,
        "deepest first, so a child's write does not re-date its parent"
    );
}

/// "Counts are `done / total`, files and bytes both." The
/// pre-flight applies the same mask the copy does, so a masked copy cannot
/// announce a denominator it will never reach.
#[test]
fn the_announced_total_counts_only_what_the_mask_lets_through() {
    let t = TempTree::new("mask-total");
    t.file("src/keep.rs", &[b'k'; 100]);
    t.file("src/drop.md", &[b'd'; 9900]);
    let dest = t.dir("dest");

    let mut spec = copy_spec(vec![t.path().join("src")], &dest);
    spec.options.file_mask = "*.rs".to_string();

    let (mut ctx, mut rx, _dtx, _flag) = JobContext::for_test(JobKind::Copy);
    crate::ops::run(&LocalFs::new(), &spec, &mut ctx);
    let summary = ctx.finish();

    let mut started = None;
    while let Ok(update) = rx.try_recv() {
        if let crate::ops::JobEvent::Started {
            files_total,
            bytes_total,
            ..
        } = update.event
        {
            started = Some((files_total, bytes_total));
        }
    }
    assert_eq!(started, Some((1, 100)), "the total is the masked total");
    assert_eq!(summary.files_done, 1);
    assert_eq!(summary.bytes_done, 100, "and the job delivered it in full");
}

/// the dialog names the file being written. `Append` used to
/// go straight to the writer, leaving the previous file's name - or, inside
/// a directory copy, the *directory's* name and its 4 KiB `st_size` - under
/// the bar while a different file was being appended to.
#[test]
fn an_append_says_which_file_it_is_appending() {
    let t = TempTree::new("append-progress");
    t.file("src/a.txt", &[b'a'; 2048]);
    let b = t.file("src/b.txt", &[b'b'; 2048]);
    let dest = t.dir("dest");
    fs::write(dest.join("b.txt"), b"old").expect("seed");

    let mut spec = copy_spec(vec![t.path().join("src/a.txt"), b], &dest);
    spec.options.conflict = Some(ConflictChoice::Append);

    let (mut ctx, mut rx, _dtx, _flag) = JobContext::for_test(JobKind::Copy);
    crate::ops::run(&LocalFs::new(), &spec, &mut ctx);
    let _ = ctx.finish();

    let mut named_b = false;
    while let Ok(update) = rx.try_recv() {
        if let crate::ops::JobEvent::Progress { file, .. } = update.event
            && file.ends_with("b.txt")
        {
            named_b = true;
        }
    }
    assert!(named_b, "the appended file was named while it was written");
}

/// the conflict policy has one meaning of `Overwrite`, and all
/// three writers now agree on it: a directory going over a file removes the
/// file, and a file going over a directory is refused rather than silently
/// failing with `EEXIST` - or, worse, removing the tree.
#[test]
fn overwrite_across_a_type_mismatch_replaces_a_file_and_refuses_a_directory() {
    let t = TempTree::new("mismatch");
    t.file("src/photos/a.jpg", b"jpeg");
    let dest = t.dir("dest");
    fs::write(dest.join("photos"), b"not a directory").expect("seed");

    let mut spec = copy_spec(vec![t.path().join("src/photos")], &dest);
    spec.options.conflict = Some(ConflictChoice::Overwrite);
    let summary = drive(spec);
    assert!(summary.is_clean(), "{:?}", summary.failures);
    assert_eq!(fs::read(dest.join("photos/a.jpg")).expect("a"), b"jpeg");

    // And the mirror: a regular file over a directory holding something.
    let t2 = TempTree::new("mismatch-2");
    let file = t2.file("thing", b"payload");
    let dest2 = t2.dir("dest");
    t2.file("dest/thing/keep.txt", b"keep");
    let mut spec = copy_spec(vec![file], &dest2);
    spec.options.conflict = Some(ConflictChoice::Overwrite);
    let summary = drive(spec);
    assert_eq!(summary.failures.len(), 1, "{:?}", summary.failures);
    assert_eq!(
        fs::read(dest2.join("thing/keep.txt")).expect("keep"),
        b"keep",
        "the directory and its contents are still there"
    );
}

#[test]
fn free_name_steps_past_the_extension() {
    let t = TempTree::new("freename");
    let a = t.file("report.txt", b"1");
    assert_eq!(free_name(&a).file_name().expect("n"), "report (2).txt");
    t.file("report (2).txt", b"2");
    assert_eq!(free_name(&a).file_name().expect("n"), "report (3).txt");

    let dotless = t.file("README", b"x");
    assert_eq!(free_name(&dotless).file_name().expect("n"), "README (2)");
}
