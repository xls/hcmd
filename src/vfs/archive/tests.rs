//! The proof of the design: an archive is a directory, and everything that
//! already worked keeps working.

use super::*;
use crate::config::ArchiveConfig;
use crate::vfs::{EntryKind, Vfs};
use std::io::Read as _;

use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A throwaway directory, removed on drop. Built by hand because `tempfile`
/// is not on the dependency table.
struct TempTree {
    root: PathBuf,
}

impl TempTree {
    fn new(tag: &str) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let root = std::env::temp_dir().join(format!(
            "hcmd-arch-{tag}-{}-{nanos:x}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).expect("create the temp tree");
        Self { root }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn session(tree: &TempTree) -> Arc<ArchiveSession> {
    ArchiveSession::in_dir(tree.path("cache"), RewriteLimits::default()).expect("session")
}

/// Write a zip with the given members.
fn write_zip(path: &Path, members: &[(&str, &[u8])]) {
    let file = std::fs::File::create(path).expect("create");
    let mut writer = ::zip::ZipWriter::new(file);
    let options = ::zip::write::SimpleFileOptions::default()
        .compression_method(::zip::CompressionMethod::Stored)
        .unix_permissions(0o644);
    for (name, body) in members {
        writer.start_file(*name, options).expect("start");
        writer.write_all(body).expect("write");
    }
    writer.finish().expect("finish");
}

/// Write a gzipped tar with the given members.
fn write_tar_gz(path: &Path, members: &[(&str, &[u8])]) {
    let file = std::fs::File::create(path).expect("create");
    let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
    let mut builder = ::tar::Builder::new(encoder);
    for (name, body) in members {
        let mut header = ::tar::Header::new_gnu();
        header.set_size(body.len() as u64);
        header.set_mode(0o644);
        header.set_mtime(1_700_000_000);
        header.set_cksum();
        builder
            .append_data(&mut header, name, std::io::Cursor::new(*body))
            .expect("append");
    }
    builder.into_inner().expect("finish").finish().expect("gz");
}

async fn listing(fs: &dyn Vfs, path: &VfsPath) -> (Vec<Entry>, Vec<String>) {
    let mut rx = fs.read_dir(path);
    let (mut rows, mut errors) = (Vec::new(), Vec::new());
    while let Some(item) = rx.recv().await {
        match item {
            Ok(entry) => rows.push(entry),
            Err(err) => errors.push(err.to_string()),
        }
    }
    (rows, errors)
}

/// `…/foo.zip#/` - the path the design says the panel shows.
fn inside(container: &Path, member: &str) -> VfsPath {
    VfsPath::local(container).with_segment(BackendKind::Archive, member)
}

#[tokio::test]
async fn entering_an_archive_lists_it_like_a_directory() {
    let tree = TempTree::new("enter");
    let container = tree.path("foo.zip");
    write_zip(
        &container,
        &[("readme.txt", b"hello"), ("src/main.rs", b"fn main() {}")],
    );
    let session = session(&tree);
    let fs = session.open(&inside(&container, "/")).expect("open");
    fs.wait_for_index();

    // "the panel path shows `…/foo.zip#/`".
    let root = inside(&container, "/");
    assert_eq!(
        root.to_string(),
        format!("{}#/", container.display()),
        "the displayed form"
    );

    let (rows, errors) = listing(fs.as_ref(), &root).await;
    assert!(errors.is_empty(), "{errors:?}");
    assert!(
        rows.first().is_some_and(|e| e.is_parent),
        "`..` comes first"
    );
    let names: Vec<&str> = rows.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"readme.txt"));
    assert!(names.contains(&"src"), "a synthesised directory");

    let readme = rows
        .iter()
        .find(|e| e.name == "readme.txt")
        .expect("readme");
    assert_eq!(readme.kind, EntryKind::File);
    assert_eq!(readme.size, 5);
    assert_eq!(readme.mode & 0o777, 0o644);
    assert!(readme.mtime.is_some());

    // And down one level, which is the same call again.
    let (rows, errors) = listing(fs.as_ref(), &inside(&container, "/src")).await;
    assert!(errors.is_empty(), "{errors:?}");
    let names: Vec<&str> = rows
        .iter()
        .filter(|e| !e.is_parent)
        .map(|e| e.name.as_str())
        .collect();
    assert_eq!(names, ["main.rs"]);
}

#[tokio::test]
async fn leaving_the_archive_lands_beside_it() {
    let tree = TempTree::new("leave");
    let container = tree.path("foo.zip");
    write_zip(&container, &[("a.txt", b"a")]);

    // `VfsPath::parent` pops the segment and lands on the
    // directory holding the container, not on the container itself.
    let root = inside(&container, "/");
    let out = root.parent().expect("out of the archive");
    assert_eq!(out, VfsPath::local(&tree.root));
    assert!(out.local_path().is_some(), "and it is addressable again");
}

#[tokio::test]
async fn a_member_reads_as_a_stream() {
    let tree = TempTree::new("read");
    let container = tree.path("a.tar.gz");
    let big = vec![b'k'; 400_000];
    write_tar_gz(
        &container,
        &[("d/small.txt", b"small"), ("d/big.bin", &big)],
    );
    let session = session(&tree);
    let fs = session.open(&inside(&container, "/")).expect("open");
    fs.wait_for_index();

    let mut reader = fs
        .open_read(&inside(&container, "/d/small.txt"))
        .expect("open small");
    let mut got = String::new();
    reader.read_to_string(&mut got).expect("read");
    assert_eq!(got, "small");

    // Larger than a pipe buffer, so this is really a stream and not a buffer.
    let mut reader = fs
        .open_read(&inside(&container, "/d/big.bin"))
        .expect("open big");
    let mut got = Vec::new();
    reader.read_to_end(&mut got).expect("read");
    assert_eq!(got, big);

    // A compressed tar is forward-only, and says so rather than pretending.
    assert!(!fs.capabilities().seekable);
    assert!(fs.open_seek(&inside(&container, "/d/small.txt")).is_err());
}

#[tokio::test]
async fn stat_answers_for_a_member_and_for_the_root() {
    let tree = TempTree::new("stat");
    let container = tree.path("a.zip");
    write_zip(&container, &[("d/f.txt", b"12345")]);
    let session = session(&tree);
    let fs = session.open(&inside(&container, "/")).expect("open");
    fs.wait_for_index();

    let root = fs.stat(&inside(&container, "/")).expect("root");
    assert!(root.is_dir());
    assert_eq!(root.name, "a.zip");

    let member = fs.stat(&inside(&container, "/d/f.txt")).expect("member");
    assert_eq!(member.size, 5);
    assert_eq!(member.name, "f.txt");

    assert!(fs.stat(&inside(&container, "/nope")).is_err());
}

#[tokio::test]
async fn a_zip_slip_entry_is_never_listed_and_cannot_be_opened() {
    let tree = TempTree::new("slip");
    let container = tree.path("evil.zip");
    write_zip(
        &container,
        &[
            ("../../../../etc/passwd", b"root::0:0"),
            ("safe.txt", b"ok"),
        ],
    );
    let session = session(&tree);
    let fs = session.open(&inside(&container, "/")).expect("open");
    fs.wait_for_index();

    let (rows, errors) = listing(fs.as_ref(), &inside(&container, "/")).await;
    let names: Vec<&str> = rows
        .iter()
        .filter(|e| !e.is_parent)
        .map(|e| e.name.as_str())
        .collect();
    assert_eq!(names, ["safe.txt"], "rejected at the door");
    assert_eq!(errors.len(), 1, "and reported: {errors:?}");
    assert!(errors[0].contains("refused"), "{errors:?}");

    // Nor can it be addressed by hand.
    assert!(fs.stat(&inside(&container, "/../../etc/passwd")).is_err());
    assert!(
        fs.open_read(&inside(&container, "/../../etc/passwd"))
            .is_err()
    );
}

#[tokio::test]
async fn a_nested_archive_is_extracted_once_and_shared() {
    let tree = TempTree::new("nested");
    let inner_path = tree.path("inner.zip");
    write_zip(&inner_path, &[("deep/file.txt", b"from the inner zip")]);
    let inner_bytes = std::fs::read(&inner_path).expect("read inner");
    std::fs::remove_file(&inner_path).expect("tidy");

    let outer = tree.path("outer.tar.gz");
    write_tar_gz(
        &outer,
        &[("nest/inner.zip", &inner_bytes), ("plain.txt", b"x")],
    );

    let session = session(&tree);
    // /outer.tar.gz#/nest/inner.zip#/deep/file.txt - the stack.
    let deep = VfsPath::local(&outer)
        .with_segment(BackendKind::Archive, "/nest/inner.zip")
        .with_segment(BackendKind::Archive, "/deep/file.txt");
    assert_eq!(
        deep.to_string(),
        format!("{}#/nest/inner.zip#/deep/file.txt", outer.display())
    );

    let fs = session.open(&deep).expect("open the inner archive");
    fs.wait_for_index();
    assert_eq!(fs.format(), format::FormatId::Zip, "detected by content");

    let mut reader = fs.open_read(&deep).expect("read through two archives");
    let mut got = String::new();
    reader.read_to_string(&mut got).expect("read");
    assert_eq!(got, "from the inner zip");

    // The second panel opening the same inner archive gets the same instance
    // and the same temp file - one extraction, one index.
    let bytes = session.cache_bytes();
    let again = session.open(&deep).expect("open again");
    assert!(Arc::ptr_eq(&fs, &again), "one archive, one index");
    assert_eq!(session.cache_bytes(), bytes, "extracted once");

    let root = std::path::PathBuf::from(session.temp_root());
    drop(fs);
    drop(again);
    drop(session);
    assert!(!root.exists(), "cleaned up on exit");
}

#[tokio::test]
async fn a_read_only_format_refuses_writes_up_front() {
    let tree = TempTree::new("readonly");
    let container = tree.path("a.rar");
    // Not a real rar - but the refusal is decided by `Capabilities`, before
    // anything is read, which is exactly the point.
    std::fs::write(&container, b"Rar!\x1a\x07\x00").expect("write");
    let session = session(&tree);
    let fs = ArchiveFs::with_format(
        &session,
        &container,
        VfsPath::local(&container),
        format::FormatId::Rar.backend(),
    );

    assert!(!fs.capabilities().writable);
    let refusal = fs.open_write(&inside(&container, "/new.txt"));
    let err = match refusal {
        Ok(_) => panic!("a .rar must refuse a write up front"),
        Err(err) => err,
    };
    assert!(err.to_string().contains("cannot be written"), "{err}");
}

#[tokio::test]
async fn a_writable_format_reports_writable_and_the_write_model() {
    let tree = TempTree::new("writable");
    let zip_path = tree.path("a.zip");
    write_zip(&zip_path, &[("a.txt", b"a")]);
    let targz = tree.path("a.tar.gz");
    write_tar_gz(&targz, &[("a.txt", b"a")]);
    let session = session(&tree);

    let zip = session.open(&inside(&zip_path, "/")).expect("zip");
    assert!(zip.capabilities().writable);
    assert_eq!(zip.write_model(), format::WriteModel::Member);

    let tgz = session.open(&inside(&targz, "/")).expect("tar.gz");
    assert!(tgz.capabilities().writable);
    assert_eq!(tgz.write_model(), format::WriteModel::FullRewrite);
}

#[tokio::test]
async fn detection_ignores_a_wrong_extension() {
    let tree = TempTree::new("misnamed");
    // "TC users routinely have archives with wrong extensions."
    let container = tree.path("notes.txt");
    write_zip(&container, &[("real.txt", b"a zip all along")]);
    let session = session(&tree);
    let fs = session.open(&inside(&container, "/")).expect("open");
    assert_eq!(fs.format(), format::FormatId::Zip);

    let tar_named_zip = tree.path("archive.zip");
    write_tar_gz(&tar_named_zip, &[("a.txt", b"a")]);
    let fs = session.open(&inside(&tar_named_zip, "/")).expect("open");
    assert_eq!(fs.format(), format::FormatId::TarGz, "content wins");
}

#[tokio::test]
async fn a_file_that_is_not_an_archive_is_refused_with_a_reason() {
    let tree = TempTree::new("notanarchive");
    let container = tree.path("notes.txt");
    std::fs::write(&container, b"just some text\n").expect("write");
    let session = session(&tree);
    let err = session
        .open(&inside(&container, "/"))
        .expect_err("not an archive");
    assert!(err.to_string().contains("not a supported archive"), "{err}");
}

#[tokio::test]
async fn a_disk_image_inside_a_compressed_file_opens_or_says_why_not() {
    // The nesting the xz work makes reachable: `disk.img.gz` holds one member,
    // and that member is a disk image. Whichever way this goes it must be
    // said - the one outcome this program does not allow is a key that
    // appears to do nothing.
    let tree = TempTree::new("imginsidegz");
    let image = crate::vfs::image::tests::fat_image(64 * 1024, &[("hello.txt", b"inside")]);
    let gz = tree.path("disk.img.gz");
    let file = std::fs::File::create(&gz).expect("create");
    let mut encoder = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
    encoder.write_all(&image).expect("write");
    encoder.finish().expect("finish");

    let inner = VfsPath::local(&gz)
        .with_segment(BackendKind::Archive, "/disk.img")
        .with_segment(BackendKind::Image, "/");
    // Through the router, which is what decides which backend a path's
    // innermost segment belongs to. `ArchiveSession::open` is the archive
    // entry point and refuses an image segment by design.
    let cfg = crate::config::Config::default();
    let router = crate::vfs::router::VfsRouter::new(cfg.archive.clone(), cfg.remote.clone());
    match router.backend_for(&inner) {
        Ok(fs_impl) => {
            let (rows, errors) = listing(fs_impl.as_ref(), &inner).await;
            assert!(errors.is_empty(), "{errors:?}");
            let names: Vec<&str> = rows
                .iter()
                .filter(|e| !e.is_parent)
                .map(|e| e.name.as_str())
                .collect();
            assert!(
                names.contains(&"hello.txt"),
                "the image inside the gzip listed nothing: {names:?}"
            );
        }
        Err(err) => {
            let said = err.to_string();
            assert!(
                !said.trim().is_empty(),
                "a refusal has to say something a reader can act on"
            );
        }
    }
}

#[tokio::test]
async fn a_singly_compressed_file_opens_as_a_container_of_one_member() {
    // It used to be refused - "a gzip stream that does not contain a tar" -
    // which is a true sentence about the implementation and an unhelpful one
    // about a compressed disk image under the cursor. One compressed file is
    // a container holding exactly one file, and then `Enter`, `F3`, `F5` and
    // `Alt+F6` all mean what they already mean.
    let tree = TempTree::new("singlestream");
    let session = session(&tree);
    let gz = tree.path("notes.txt.gz");
    let body = b"not a tar, just text";
    let file = std::fs::File::create(&gz).expect("create");
    let mut encoder = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
    encoder.write_all(body).expect("write");
    encoder.finish().expect("finish");

    let fs_impl = session
        .open(&inside(&gz, "/"))
        .expect("a one-member container");
    assert_eq!(fs_impl.format(), format::FormatId::Gz);
    let (rows, errors) = listing(fs_impl.as_ref(), &inside(&gz, "/")).await;
    assert!(errors.is_empty(), "{errors:?}");
    let members: Vec<&Entry> = rows.iter().filter(|e| !e.is_parent).collect();
    let names: Vec<&str> = members.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["notes.txt"],
        "the member is the container's name without the compression suffix"
    );
    assert_eq!(
        members[0].size,
        body.len() as u64,
        "gzip states its uncompressed size in its last four bytes"
    );

    let mut reader = fs_impl
        .open_read(&inside(&gz, "/notes.txt"))
        .expect("read the member");
    let mut got = Vec::new();
    std::io::Read::read_to_end(&mut reader, &mut got).expect("to the end");
    assert_eq!(got, body, "the member decompresses byte for byte");
}

#[tokio::test]
async fn the_listing_streams_rather_than_waiting_for_the_index() {
    let tree = TempTree::new("streaming");
    let container = tree.path("many.tar.gz");
    let members: Vec<(String, Vec<u8>)> = (0..2000)
        .map(|i| (format!("f{i:05}.txt"), vec![b'x'; 512]))
        .collect();
    let borrowed: Vec<(&str, &[u8])> = members
        .iter()
        .map(|(name, body)| (name.as_str(), body.as_slice()))
        .collect();
    write_tar_gz(&container, &borrowed);

    let session = session(&tree);
    let fs = session.open(&inside(&container, "/")).expect("open");
    // Deliberately *not* waiting for the index: the first rows must arrive
    // while it is still being built.
    let mut rx = fs.read_dir(&inside(&container, "/"));
    let first = rx.recv().await.expect("a row").expect("not an error");
    assert!(first.is_parent, "`..` before anything has been read");
    let second = rx.recv().await.expect("a row").expect("not an error");
    assert!(second.name.starts_with('f'));

    let mut count = 2;
    while let Some(item) = rx.recv().await {
        if item.is_ok() {
            count += 1;
        }
    }
    assert_eq!(count, 2001, "every member and the `..` row");
}

#[tokio::test]
async fn dropping_the_listing_cancels_it() {
    let tree = TempTree::new("cancel");
    let container = tree.path("many.zip");
    let bodies: Vec<(String, Vec<u8>)> = (0..500)
        .map(|i| (format!("f{i:04}.txt"), vec![b'y'; 64]))
        .collect();
    let borrowed: Vec<(&str, &[u8])> = bodies
        .iter()
        .map(|(name, body)| (name.as_str(), body.as_slice()))
        .collect();
    write_zip(&container, &borrowed);

    let session = session(&tree);
    let fs = session.open(&inside(&container, "/")).expect("open");
    let mut rx = fs.read_dir(&inside(&container, "/"));
    assert!(rx.recv().await.is_some());
    drop(rx);
    tokio::task::yield_now().await;
    // Nothing panicked, and the archive is still usable.
    let (rows, _) = listing(fs.as_ref(), &inside(&container, "/")).await;
    assert_eq!(rows.len(), 501);
}

#[tokio::test]
async fn closing_the_archive_stops_the_index_build() {
    let tree = TempTree::new("stop");
    let container = tree.path("a.zip");
    write_zip(&container, &[("a.txt", b"a")]);
    let session = session(&tree);
    let fs = session.open(&inside(&container, "/")).expect("open");
    let index = Arc::clone(fs.index());
    let key = ArchiveSession::key_for(&fs).expect("key");
    assert!(!index.cancelled());
    drop(fs);
    // The session still holds it, so it is still open.
    assert!(!index.cancelled());
    session.close(&key);
    // Now nothing holds it and the build has been told to stop.
    assert!(index.cancelled(), "closing stops the read");
}

#[test]
fn the_session_is_built_from_the_archive_configuration() {
    let config = ArchiveConfig {
        temp_dir: std::env::temp_dir().to_string_lossy().into_owned(),
        ..ArchiveConfig::default()
    };
    let session = ArchiveSession::new(&config).expect("session");
    assert!(session.temp_root().starts_with(std::env::temp_dir()));
    assert_eq!(session.limits().warn, config.rewrite_warn_size.bytes());
    assert_eq!(session.limits().max, config.rewrite_max_size.bytes());
}

#[tokio::test]
async fn the_viewer_reads_a_member_through_the_opener_it_uses_for_a_file() {
    // The claim the design makes for the trait, tested rather than asserted:
    // `F3` inside an archive is v0.4's viewer, unchanged, over v0.5's backend.
    let tree = TempTree::new("viewer");
    let container = tree.path("a.tar.gz");
    let body: Vec<u8> = (0..200_000u32)
        .map(|i| b'a'.wrapping_add((i % 26) as u8))
        .collect();
    write_tar_gz(&container, &[("notes.txt", &body)]);
    let session = session(&tree);
    let fs = session.open(&inside(&container, "/")).expect("open");
    fs.wait_for_index();

    let path = inside(&container, "/notes.txt");
    let entry = fs.stat(&path).expect("stat");
    let vfs: Arc<dyn Vfs> = Arc::clone(&fs) as Arc<dyn Vfs>;
    let opener = crate::viewer::source::vfs_opener(vfs, path);
    let mut source = crate::viewer::source::Source::open(opener, Some(entry.size)).expect("open");

    let head = source
        .read_window(0, crate::viewer::source::WindowLen::new(16))
        .expect("head");
    assert_eq!(head.bytes(), body.get(..16).expect("head"));

    // Forward, then *backward* - which on a stream costs a reopen and a
    // replay, and is the path a compressed member forces.
    let far = source
        .read_window(150_000, crate::viewer::source::WindowLen::new(32))
        .expect("far");
    assert_eq!(far.bytes(), body.get(150_000..150_032).expect("far"));
    let back = source
        .read_window(64, crate::viewer::source::WindowLen::new(8))
        .expect("back");
    assert_eq!(back.bytes(), body.get(64..72).expect("back"));
}

#[tokio::test]
async fn a_member_that_is_a_file_is_not_a_directory() {
    let tree = TempTree::new("notadir");
    let container = tree.path("a.zip");
    write_zip(&container, &[("f.txt", b"x")]);
    let session = session(&tree);
    let fs = session.open(&inside(&container, "/")).expect("open");
    fs.wait_for_index();

    let (rows, errors) = listing(fs.as_ref(), &inside(&container, "/f.txt")).await;
    assert!(rows.iter().all(|e| e.is_parent), "nothing to list");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].contains("not a directory"), "{errors:?}");

    let (_, errors) = listing(fs.as_ref(), &inside(&container, "/nope")).await;
    assert!(errors[0].contains("not found"), "{errors:?}");
}

#[tokio::test]
async fn an_empty_directory_member_lists_as_empty_rather_than_missing() {
    let tree = TempTree::new("emptydir");
    let container = tree.path("a.zip");
    write_zip(&container, &[("empty/", b""), ("f.txt", b"x")]);
    let session = session(&tree);
    let fs = session.open(&inside(&container, "/")).expect("open");
    fs.wait_for_index();

    let (rows, errors) = listing(fs.as_ref(), &inside(&container, "/empty")).await;
    assert!(
        errors.is_empty(),
        "an empty directory is not an error: {errors:?}"
    );
    assert_eq!(rows.len(), 1, "only the `..` row");
    assert!(rows[0].is_parent);
}

// ---------------------------------------------------------------------------
// the design through `ops`: the operations, not the backend
//
// Everything below drives `crate::ops::run` - the same engine `F5`, `F8`,
// `Alt+F5` and `Alt+F6` reach - over a `VfsRouter`, which is what the running
// application hands it. That is the claim the design makes ("archives, search
// results and (later) remote filesystems are uniform to the panel") tested
// where it either holds or does not: no test here names a format, and `ops`
// contains no code that does.
// ---------------------------------------------------------------------------

use crate::ops::{JobContext, JobKind, JobSpec, JobSummary};
use crate::vfs::VfsRouter;

/// Run one job over a router, off the runtime's worker threads.
///
/// `spawn_blocking` for the same reason `ops::spawn` uses it: the job engine
/// blocks, and it drains `Vfs::read_dir` with `blocking_recv`, which a runtime
/// worker may not do.
async fn run_job(router: Arc<VfsRouter>, spec: JobSpec) -> JobSummary {
    tokio::task::spawn_blocking(move || {
        let (mut ctx, mut rx, dtx, _flag) = JobContext::for_test(spec.kind);
        // No UI to answer a conflict, so the question has to end rather than
        // park: `JobContext::ask` reads a closed decision channel as "the UI
        // went away" and unwinds the batch, which is the answer that touches
        // nothing. A test that wanted a conflict answered would hold this and
        // send one.
        drop(dtx);
        // The update channel is **drained**, because it is bounded and
        // `JobContext::send` blocks on it: a job whose progress nobody reads
        // stops at the sixty-fourth event and never comes back. The real
        // application always has a reader (`ops::spawn`), so a test that does
        // not is testing a shape that cannot happen.
        let updates = std::thread::spawn(move || while rx.blocking_recv().is_some() {});
        crate::ops::run(router.as_ref(), &spec, &mut ctx);
        let summary = ctx.finish();
        updates.join().expect("the update drain");
        summary
    })
    .await
    .expect("the job thread")
}

/// A router whose archive session lives **inside this test's own tree**.
///
/// Not `$TMPDIR`: creating a session sweeps its temp base for orphans,
/// and a sweep is exactly what one parallel test must not run
/// over another's live session.
fn router(tree: &TempTree) -> Arc<VfsRouter> {
    let base = tree.path("session");
    std::fs::create_dir_all(&base).expect("session base");
    Arc::new(VfsRouter::new(
        ArchiveConfig {
            temp_dir: base.to_string_lossy().into_owned(),
            ..ArchiveConfig::default()
        },
        crate::config::RemoteConfig::default(),
    ))
}

#[tokio::test]
async fn alt_f6_extracts_a_whole_archive_through_the_copy_engine() {
    // "`Alt+F6` unpacks the archive under the cursor to the
    // other panel's directory", which `App::unpack_under_cursor` asks for as an
    // ordinary `JobKind::Copy` from `<archive>#/`. The tree comes out with its
    // directories, and the bytes are the bytes.
    let tree = TempTree::new("altf6");
    let container = tree.path("bundle.tar.gz");
    write_tar_gz(
        &container,
        &[("hello.txt", b"hello"), ("sub/deep.txt", b"deeper")],
    );
    let dest = tree.path("out");
    std::fs::create_dir_all(&dest).expect("mkdir");

    let summary = run_job(
        router(&tree),
        JobSpec::new(
            JobKind::Copy,
            vec![inside(&container, "/")],
            Some(VfsPath::local(&dest)),
        ),
    )
    .await;

    assert!(summary.is_clean(), "{:?}", summary.failures);
    assert_eq!(
        std::fs::read(dest.join("hello.txt")).expect("hello"),
        b"hello"
    );
    assert_eq!(
        std::fs::read(dest.join("sub/deep.txt")).expect("deep"),
        b"deeper"
    );
}

#[tokio::test]
async fn f5_copies_one_member_out_and_leaves_no_partial_behind() {
    // "`F5` **out of** an archive extracts, with full progress and conflict
    // handling" - which means the ordinary copy engine, and so the ordinary
    // the design promise about what is on disk afterwards.
    let tree = TempTree::new("f5out");
    let container = tree.path("a.zip");
    write_zip(&container, &[("notes.txt", b"the member's bytes")]);
    let dest = tree.path("out");
    std::fs::create_dir_all(&dest).expect("mkdir");

    let summary = run_job(
        router(&tree),
        JobSpec::new(
            JobKind::Copy,
            vec![inside(&container, "/notes.txt")],
            Some(VfsPath::local(&dest)),
        ),
    )
    .await;

    assert!(summary.is_clean(), "{:?}", summary.failures);
    assert_eq!(
        std::fs::read(dest.join("notes.txt")).expect("read"),
        b"the member's bytes"
    );
    let leftovers: Vec<String> = std::fs::read_dir(&dest)
        .expect("read_dir")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n != "notes.txt")
        .collect();
    assert!(leftovers.is_empty(), "no partial file: {leftovers:?}");
}

#[tokio::test]
async fn copying_one_member_onto_an_existing_name_raises_the_conflict() {
    // the design does not exempt the root of a copy from the conflict
    // dialog, and the root is the one the user actually named. With no answer
    // available the batch unwinds, which is the safe direction: nothing on
    // disk is touched.
    let tree = TempTree::new("rootclash");
    let container = tree.path("a.zip");
    write_zip(&container, &[("notes.txt", b"from the archive")]);
    let dest = tree.path("out");
    std::fs::create_dir_all(&dest).expect("mkdir");
    std::fs::write(dest.join("notes.txt"), b"already here").expect("write");

    let summary = run_job(
        router(&tree),
        JobSpec::new(
            JobKind::Copy,
            vec![inside(&container, "/notes.txt")],
            Some(VfsPath::local(&dest)),
        ),
    )
    .await;

    // The test harness answers nothing, so the question unwinds the batch.
    assert_eq!(
        std::fs::read(dest.join("notes.txt")).expect("read"),
        b"already here",
        "the existing file was not overwritten without being asked about"
    );
    assert_eq!(summary.files_done, 0);
}

#[tokio::test]
async fn f5_into_an_archive_adds_a_member() {
    // "`F5` **into** an archive adds, only for backends whose
    // `Capabilities` report writability."
    let tree = TempTree::new("f5in");
    let container = tree.path("a.zip");
    write_zip(&container, &[("kept.txt", b"already here")]);
    let source = tree.path("added.txt");
    std::fs::write(&source, b"from the panel").expect("write");

    let summary = run_job(
        router(&tree),
        JobSpec::new(
            JobKind::Copy,
            vec![VfsPath::local(&source)],
            Some(inside(&container, "/")),
        ),
    )
    .await;
    assert!(summary.is_clean(), "{:?}", summary.failures);

    // Read it back through the backend rather than through the zip crate: what
    // matters is that the panel will show it.
    let session = session(&tree);
    let fs = session.open(&inside(&container, "/")).expect("open");
    fs.wait_for_index();
    let (rows, errors) = listing(fs.as_ref(), &inside(&container, "/")).await;
    assert!(errors.is_empty(), "{errors:?}");
    let names: Vec<&str> = rows.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"added.txt"), "{names:?}");
    assert!(names.contains(&"kept.txt"), "the old member survived");

    let mut body = String::new();
    fs.open_read(&inside(&container, "/added.txt"))
        .expect("open")
        .read_to_string(&mut body)
        .expect("read");
    assert_eq!(body, "from the panel");
}

#[tokio::test]
async fn f8_inside_an_archive_deletes_from_it() {
    // "`F8` inside an archive deletes from it, same capability
    // rules." A whole subtree goes, and what was not named stays.
    let tree = TempTree::new("f8in");
    let container = tree.path("a.zip");
    write_zip(
        &container,
        &[
            ("keep.txt", b"kept"),
            ("sub/one.txt", b"one"),
            ("sub/two.txt", b"two"),
        ],
    );

    let summary = run_job(
        router(&tree),
        JobSpec::new(
            JobKind::Delete { trash: true },
            vec![inside(&container, "/sub")],
            None,
        ),
    )
    .await;
    assert!(summary.is_clean(), "{:?}", summary.failures);

    let session = session(&tree);
    let fs = session.open(&inside(&container, "/")).expect("open");
    fs.wait_for_index();
    let (rows, errors) = listing(fs.as_ref(), &inside(&container, "/")).await;
    assert!(errors.is_empty(), "{errors:?}");
    let names: Vec<&str> = rows.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"keep.txt"), "{names:?}");
    assert!(!names.contains(&"sub"), "the subtree is gone: {names:?}");
}

#[tokio::test]
async fn f8_inside_an_archive_is_permanent_and_the_split_says_so() {
    // There is no rename that puts a member of a `.zip` into
    // `~/.local/share/Trash/files`, so the split has to call it
    // untrashable - which is what makes the confirmation say "permanently"
    // *before* it happens rather than after.
    let tree = TempTree::new("f8split");
    let container = tree.path("a.zip");
    write_zip(&container, &[("f.txt", b"x")]);
    let split = crate::ops::delete::split_by_trash(&[inside(&container, "/f.txt")]);
    assert!(split.trashable.is_empty(), "{split:?}");
    assert_eq!(split.untrashable.len(), 1);
}

#[tokio::test]
async fn a_selection_inside_an_archive_can_be_sized() {
    // the `Space`/`Ctrl+L`, asked of a directory that happens to be inside a
    // container. Until v0.5 this reported "cannot be sized until v0.5"; a
    // figure that is exact is now available, and the design forbids
    // reporting one that only looks it.
    let tree = TempTree::new("size");
    let container = tree.path("a.zip");
    write_zip(
        &container,
        &[("sub/one.txt", b"12345"), ("sub/two.txt", b"678")],
    );

    let summary = run_job(
        router(&tree),
        JobSpec::size(vec![inside(&container, "/sub")]),
    )
    .await;
    assert!(summary.is_clean(), "{:?}", summary.failures);
    assert_eq!(summary.bytes_done, 8, "5 + 3");
    assert_eq!(summary.files_done, 2);
}

#[tokio::test]
async fn a_member_that_lies_about_its_size_is_stopped_at_what_it_declared() {
    // the bomb defence, reached the way every byte of every member
    // is reached: `Vfs::open_read`. The header says sixteen bytes and the
    // stream does not stop, so the read does - and the copy fails rather than
    // filling the filesystem.
    let tree = TempTree::new("liar");
    let container = tree.path("liar.zip");
    // A stored member whose local header understates it. Written by hand: the
    // `zip` crate will not produce a header that disagrees with its data.
    let body = vec![b'A'; 4096];
    write_zip(&container, &[("big.txt", &body)]);

    let session = session(&tree);
    let fs = session.open(&inside(&container, "/")).expect("open");
    fs.wait_for_index();
    let member = fs
        .index()
        .get("big.txt")
        .expect("the member is in the index");

    // The guard is charged against what the *index* recorded, so shrinking the
    // recorded size is the same lie a hostile header tells.
    let mut lying = member.clone();
    lying.size = 16;
    let mut sink = Vec::new();
    let outcome = crate::vfs::archive::zip::ZipFormat.read_member(
        &container,
        &member,
        &mut safety::GuardedWriter::new(
            &mut sink,
            safety::Guard::for_member(&lying.path, Some(16), safety::ExtractLimits::default()),
        ),
    );
    assert!(outcome.is_err(), "refused: {outcome:?}");
    assert!(sink.len() <= 16, "and stopped there: {}", sink.len());
}

#[tokio::test]
async fn the_root_member_of_a_tar_is_not_reported_as_an_unsafe_entry() {
    // `tar -czf x.tgz .` opens with a member called `./`, which names the
    // archive's own root. It is not an escape and must not be counted as one:
    // the refusal counter exists for Zip Slip, and a warning on the
    // most ordinary tar there is teaches the user to ignore it.
    let tree = TempTree::new("dotroot");
    let container = tree.path("dot.tar.gz");
    {
        let file = std::fs::File::create(&container).expect("create");
        let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
        let mut builder = ::tar::Builder::new(encoder);
        let mut header = ::tar::Header::new_gnu();
        header.set_size(0);
        header.set_mode(0o755);
        header.set_entry_type(::tar::EntryType::Directory);
        header.set_cksum();
        builder
            .append_data(&mut header, "./", std::io::empty())
            .expect("append the root");
        let mut header = ::tar::Header::new_gnu();
        header.set_size(3);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, "./a.txt", std::io::Cursor::new(b"abc"))
            .expect("append");
        builder.into_inner().expect("finish").finish().expect("gz");
    }

    let session = session(&tree);
    let fs = session.open(&inside(&container, "/")).expect("open");
    fs.wait_for_index();
    assert_eq!(fs.index().refusals().0, 0, "nothing was refused");

    let (rows, errors) = listing(fs.as_ref(), &inside(&container, "/")).await;
    assert!(errors.is_empty(), "and nothing was reported: {errors:?}");
    let names: Vec<&str> = rows.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"a.txt"), "{names:?}");
}

#[test]
fn an_empty_tar_is_an_archive() {
    // It is what `tar cf empty.tar --files-from=/dev/null` writes and what
    // `tar tf` reads back without complaint - and it is what the design's
    // `Alt+F5` creates before it puts anything in. Refusing to open one was a
    // bug on its own.
    let tree = TempTree::new("emptytar");

    let plain = tree.path("empty.tar");
    std::fs::write(&plain, [0u8; 1024]).expect("write");
    assert_eq!(format::detect(&plain).expect("plain"), FormatId::Tar);

    let gz = tree.path("empty.tar.gz");
    {
        let file = std::fs::File::create(&gz).expect("create");
        let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
        ::tar::Builder::new(encoder)
            .into_inner()
            .expect("finish")
            .finish()
            .expect("gz");
    }
    assert_eq!(format::detect(&gz).expect("gz"), FormatId::TarGz);

    // And the bound still holds: a large run of zeros is not an empty tar.
    let big = tree.path("zeros.img");
    std::fs::write(&big, vec![0u8; 2 * 1024 * 1024]).expect("write");
    assert!(format::detect(&big).is_err(), "not an archive");
}

/// A panicking indexer must still leave the index in a final state.
///
/// Everything that consumes an archive listing - `stat_blocking`,
/// `wait_until_final`, and the `read_dir` task that streams rows to the panel -
/// loops until the status is final. An indexing thread that ended without
/// setting one would block all three for the life of the process: a panel
/// showing its `..` row and nothing else, for ever, with no error to report.
///
/// Archive data is attacker-controlled, so "the indexer never
/// panics" is a property worth having but not one a hung panel may depend on.
#[test]
fn an_indexer_that_dies_still_ends_the_build() {
    let index = Arc::new(Index::new());
    let worker = Arc::clone(&index);
    let epoch = index.epoch();

    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let died = std::thread::spawn(move || {
        let _guard = FinishOnDrop {
            index: Arc::clone(&worker),
            epoch,
        };
        panic!("a corrupt header took the indexer down");
    })
    .join();
    std::panic::set_hook(previous);
    assert!(died.is_err(), "the thread really did panic");

    // The point: this returns rather than blocking for ever.
    match index.wait_until_final() {
        IndexStatus::Failed(why) => {
            assert!(why.contains("stopped unexpectedly"), "{why}");
        }
        other => panic!("a dead indexer must leave a failure, got {other:?}"),
    }
    assert!(
        index.status().is_final(),
        "a waiter arriving later must not block either"
    );
}

/// The same hole on the member-reading side: a worker that dies part way
/// through must not look like a member that simply ended.
///
/// The pipe gives the reader EOF whichever way the worker leaves, so without a
/// recorded failure a half-written member is indistinguishable from a complete
/// one - an extraction would write the truncated bytes and report success.
#[test]
fn a_member_worker_that_dies_is_an_error_not_a_short_file() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let mut reader = stream::piped(|out| {
        out.write_all(b"the first half").map_err(Error::Bare)?;
        panic!("a lying size took the reader down");
    })
    .expect("spawn");

    let mut got = Vec::new();
    let outcome = reader.read_to_end(&mut got);
    std::panic::set_hook(previous);

    assert_eq!(got, b"the first half", "what did arrive is still readable");
    let err = outcome.expect_err("a truncated member is not a successful read");
    assert!(err.to_string().contains("stopped unexpectedly"), "{err}");
}

/// Hostile and malformed containers, driven through the whole read path.
///
/// the archives are attacker-controlled input: a header can lie
/// about a size, an offset can point past the end, a name can be bytes that
/// are not UTF-8, a count can be larger than memory. Every one of those is an
/// `Err` or an empty listing; none of them is a panic, an allocation sized
/// from the file, or a wait that never ends.
///
/// The assertion is the absence of a crash and the presence of an answer:
/// each container is detected, opened, listed, statted and read, and the test
/// passes if every one of those returns.
#[tokio::test]
async fn malformed_containers_are_refused_rather_than_fatal() {
    let tree = TempTree::new("hostile");

    // A zip end-of-central-directory record claiming 65535 members in a file
    // that holds none: the count is the classic allocate-from-the-header bug.
    let mut lying_eocd = b"PK\x05\x06".to_vec();
    lying_eocd.extend_from_slice(&0u16.to_le_bytes()); // disk
    lying_eocd.extend_from_slice(&0u16.to_le_bytes()); // cd start disk
    lying_eocd.extend_from_slice(&u16::MAX.to_le_bytes()); // entries on disk
    lying_eocd.extend_from_slice(&u16::MAX.to_le_bytes()); // entries total
    lying_eocd.extend_from_slice(&u32::MAX.to_le_bytes()); // cd size
    lying_eocd.extend_from_slice(&u32::MAX.to_le_bytes()); // cd offset
    lying_eocd.extend_from_slice(&0u16.to_le_bytes()); // comment length

    // Two hand-built tar headers. `poke` writes a fixed field without any
    // indexing that could panic on a short buffer.
    fn poke(block: &mut [u8], at: usize, bytes: &[u8]) {
        if let Some(slot) = block.get_mut(at..at.saturating_add(bytes.len())) {
            slot.copy_from_slice(bytes);
        }
    }
    /// Sign a header the way tar does: the checksum field counts as spaces.
    fn sign(block: &mut [u8]) {
        let sum: u32 = block
            .iter()
            .enumerate()
            .map(|(i, b)| {
                if (148..156).contains(&i) {
                    32
                } else {
                    u32::from(*b)
                }
            })
            .sum();
        poke(block, 148, format!("{sum:06o}\0 ").as_bytes());
    }

    // A size field holding the largest octal it can, so the padded end of the
    // entry overflows a u64 unless the arithmetic saturates.
    let mut giant_tar = vec![0u8; 512];
    poke(&mut giant_tar, 0, b"giant.bin");
    poke(&mut giant_tar, 124, b"77777777777 ");
    poke(&mut giant_tar, 257, b"ustar\0");
    sign(&mut giant_tar);

    // A member name that is not UTF-8 at all.
    let mut latin1_tar = vec![0u8; 512];
    poke(&mut latin1_tar, 0, &[0xff, 0xfe, 0x80, b'.', b'z']);
    poke(&mut latin1_tar, 124, b"00000000000 ");
    poke(&mut latin1_tar, 257, b"ustar\0");
    sign(&mut latin1_tar);

    // A gzip member whose header is right and whose deflate stream is noise.
    let mut broken_gz = vec![0x1f, 0x8b, 0x08, 0, 0, 0, 0, 0, 0, 0x03];
    broken_gz.extend_from_slice(&[0xde, 0xad, 0xbe, 0xef, 0x99, 0x11, 0x22]);

    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("empty.zip", Vec::new()),
        ("lying-count.zip", lying_eocd),
        ("truncated-magic.zip", b"PK\x03\x04".to_vec()),
        ("giant-size.tar", giant_tar),
        ("not-utf8-name.tar", latin1_tar),
        ("noise.tar", vec![0xa5; 4096]),
        ("half-header.tar", vec![0u8; 300]),
        ("broken-stream.tar.gz", broken_gz),
        ("truncated.tar.gz", vec![0x1f, 0x8b, 0x08]),
        ("bare-magic.7z", b"7z\xbc\xaf\x27\x1c".to_vec()),
        ("bare-magic.rar", b"Rar!\x1a\x07\x01\x00".to_vec()),
        ("bare-magic.tar.zst", vec![0x28, 0xb5, 0x2f, 0xfd]),
        ("bare-magic.tar.bz2", b"BZh9".to_vec()),
        (
            "bare-magic.tar.xz",
            vec![0xfd, b'7', b'z', b'X', b'Z', 0x00],
        ),
    ];

    for (name, bytes) in cases {
        let container = tree.path(name);
        std::fs::write(&container, &bytes).expect("write the container");

        // Detection reads the file's own bytes and must answer either way.
        let _ = format::detect(&container);

        let session = session(&tree);
        let Ok(fs) = session.open(&inside(&container, "/")) else {
            // Refused up front, which is a perfectly good answer.
            continue;
        };
        let status = fs.wait_for_index();
        assert!(
            status.is_final(),
            "{name}: the build must end, not hang the panel"
        );

        let (rows, _errors) = listing(fs.as_ref(), &inside(&container, "/")).await;
        for row in &rows {
            if row.name == ".." {
                continue;
            }
            let path = inside(&container, &format!("/{}", row.name));
            let _ = fs.stat(&path);
            if let Ok(mut reader) = fs.open_read(&path) {
                // Bounded: a member that claims more than this is the bomb
                // case, and the caps are safety.rs's business.
                let mut sink = Vec::new();
                let _ = std::io::copy(&mut (&mut reader).take(1 << 20), &mut sink);
            }
        }

        // A name no listing produced, to exercise the not-found path against a
        // container that may never have indexed at all.
        let _ = fs.stat(&inside(&container, "/no/such/member"));
        if let Ok(key) = ArchiveSession::key_for(&fs) {
            session.close(&key);
        }
    }
}

// ---------------------------------------------------------------------------
// Regressions: the threat model, and the archives it damaged
//
// Every fixture below is **crafted in the test**, header by header where the
// library will not write what a hostile or merely old container holds. Each
// one reproduced a defect that reached the disk: a file the archive never
// contained, a setuid binary, a member silently dropped, a name silently
// rewritten, an archive silently not written at all.
// ---------------------------------------------------------------------------

/// What a hand-built tar entry is.
enum Craft<'a> {
    /// A regular file, with the mode the header records.
    File(&'a str, &'a [u8], u32),
    /// A regular file whose **name** is raw bytes: a tar stores names with no
    /// declared encoding, and `tar::Builder` will not write one that is not
    /// UTF-8.
    RawName(&'a [u8], &'a [u8]),
    /// A symbolic link, whose target lives in the header and **not** in the
    /// member's data.
    Symlink(&'a str, &'a str),
    /// A hard link to another member of the same archive.
    Hardlink(&'a str, &'a str),
    /// A file header that declares a size the container cannot possibly hold.
    Bomb(&'a str, u64),
}

/// Write `bytes` straight into the header at `at`.
///
/// `tar::Builder::append_data` normalises and refuses names, which is the
/// library being careful and exactly why it cannot be the fixture: a hostile
/// tar is not written by `tar::Builder`.
fn poke(header: &mut ::tar::Header, at: usize, bytes: &[u8]) {
    let raw = header.as_mut_bytes();
    for (i, byte) in bytes.iter().enumerate() {
        if let Some(slot) = raw.get_mut(at.saturating_add(i)) {
            *slot = *byte;
        }
    }
}

/// The bytes of a `.tar` holding exactly these entries.
fn craft_tar_bytes(items: &[Craft<'_>]) -> Vec<u8> {
    let mut builder = ::tar::Builder::new(Vec::new());
    for item in items {
        let mut header = ::tar::Header::new_gnu();
        header.set_mtime(1_700_000_000);
        header.set_uid(0);
        header.set_gid(0);
        header.set_mode(0o644);
        header.set_size(0);
        let body: &[u8] = match item {
            Craft::File(name, body, mode) => {
                header.set_entry_type(::tar::EntryType::Regular);
                header.set_mode(*mode);
                header.set_size(body.len() as u64);
                poke(&mut header, 0, name.as_bytes());
                body
            }
            Craft::RawName(name, body) => {
                header.set_entry_type(::tar::EntryType::Regular);
                header.set_size(body.len() as u64);
                poke(&mut header, 0, name);
                body
            }
            Craft::Symlink(name, target) => {
                header.set_entry_type(::tar::EntryType::Symlink);
                header.set_mode(0o777);
                poke(&mut header, 0, name.as_bytes());
                poke(&mut header, 157, target.as_bytes());
                &[]
            }
            Craft::Hardlink(name, target) => {
                header.set_entry_type(::tar::EntryType::Link);
                poke(&mut header, 0, name.as_bytes());
                poke(&mut header, 157, target.as_bytes());
                &[]
            }
            Craft::Bomb(name, declared) => {
                header.set_entry_type(::tar::EntryType::Regular);
                // The header's claim, with nothing behind it. Nothing may
                // allocate, reserve or write on the strength of this number.
                header.set_size(*declared);
                poke(&mut header, 0, name.as_bytes());
                &[]
            }
        };
        header.set_cksum();
        builder.append(&header, body).expect("append");
    }
    builder.into_inner().expect("finish")
}

fn craft_tar(path: &Path, items: &[Craft<'_>]) {
    std::fs::write(path, craft_tar_bytes(items)).expect("write the fixture");
}

fn craft_tar_gz(path: &Path, items: &[Craft<'_>]) {
    let file = std::fs::File::create(path).expect("create");
    let mut encoder = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
    encoder
        .write_all(&craft_tar_bytes(items))
        .expect("compress");
    encoder.finish().expect("gz");
}

/// The name bytes of every member of a `.tar.gz`, exactly as stored.
fn raw_tar_gz_names(path: &Path) -> Vec<Vec<u8>> {
    let file = std::fs::File::open(path).expect("open");
    let mut archive = ::tar::Archive::new(flate2::read::GzDecoder::new(file));
    archive
        .entries()
        .expect("entries")
        .map(|e| e.expect("entry").path_bytes().into_owned())
        .collect()
}

#[tokio::test]
async fn a_setuid_member_does_not_unpack_as_a_setuid_file() {
    // the threat model, and the attack `safety::safe_mode`'s own
    // documentation names: "an archive that unpacks a setuid-root shell is a
    // machine given away". `preserve_attrs` is on by default (the design's
    // "Preservation: mode"), so `Alt+F6` applied the header's mode verbatim
    // and dropped a setuid binary, owned by the person unpacking, into the
    // other panel's directory.
    let tree = TempTree::new("setuid");
    let container = tree.path("evil.tar");
    craft_tar(
        &container,
        &[Craft::File("rootme", b"#!/bin/sh\nid\n", 0o4755)],
    );
    let dest = tree.path("out");
    std::fs::create_dir_all(&dest).expect("mkdir");

    let summary = run_job(
        router(&tree),
        JobSpec::new(
            JobKind::Copy,
            vec![inside(&container, "/")],
            Some(VfsPath::local(&dest)),
        ),
    )
    .await;
    assert!(summary.is_clean(), "{:?}", summary.failures);

    use std::os::unix::fs::PermissionsExt as _;
    let mode = std::fs::metadata(dest.join("rootme"))
        .expect("the extracted file")
        .permissions()
        .mode();
    assert_eq!(
        mode & 0o7000,
        0,
        "setuid, setgid and the sticky bit do not survive an unpack \
; got {:o}",
        mode & 0o7777
    );
    assert_eq!(
        mode & 0o777,
        0o755,
        "and everything else the archive asked for is still preserved \
"
    );
}

#[tokio::test]
async fn a_tar_symlink_extracts_as_a_link_from_its_header() {
    // A tar keeps a symlink's target in the **header**, and gives the member
    // no data at all. Reading the member's contents to find the target - the
    // zip convention - therefore read zero bytes, and every symlink in every
    // ordinary source tarball failed to extract with "an empty link target".
    // `Vfs::read_link` is the backend answering the question instead.
    //
    let tree = TempTree::new("tarlink");
    let container = tree.path("links.tar");
    craft_tar(
        &container,
        &[
            Craft::File("real.txt", b"the file it points at", 0o644),
            Craft::Symlink("soft.txt", "real.txt"),
        ],
    );
    let dest = tree.path("out");
    std::fs::create_dir_all(&dest).expect("mkdir");

    let summary = run_job(
        router(&tree),
        JobSpec::new(
            JobKind::Copy,
            vec![inside(&container, "/")],
            Some(VfsPath::local(&dest)),
        ),
    )
    .await;
    assert!(summary.is_clean(), "{:?}", summary.failures);

    let link = dest.join("soft.txt");
    let meta = std::fs::symlink_metadata(&link).expect("the extracted link");
    assert!(
        meta.file_type().is_symlink(),
        "symlinks are copied as links by default"
    );
    assert_eq!(
        std::fs::read_link(&link).expect("read_link"),
        Path::new("real.txt"),
        "and they point where the archive said"
    );
}

#[tokio::test]
async fn a_tar_hard_link_extracts_the_bytes_it_links_to() {
    // A tar hard-link entry has a header and no data: its declared size is
    // zero and the bytes are the target's. Handing it out as it stood wrote a
    // zero-byte file and reported the job complete - a copy that neither
    // completed nor said why. The link is resolved in the
    // backend, so `ops` still has no idea what a tar is.
    let tree = TempTree::new("tarhard");
    let container = tree.path("links.tar");
    let body = b"thirty-two bytes of real content";
    craft_tar(
        &container,
        &[
            Craft::File("original.txt", body, 0o644),
            Craft::Hardlink("hardlink.txt", "original.txt"),
        ],
    );
    let dest = tree.path("out");
    std::fs::create_dir_all(&dest).expect("mkdir");

    let summary = run_job(
        router(&tree),
        JobSpec::new(
            JobKind::Copy,
            vec![inside(&container, "/")],
            Some(VfsPath::local(&dest)),
        ),
    )
    .await;
    assert!(summary.is_clean(), "{:?}", summary.failures);
    assert_eq!(
        std::fs::read(dest.join("hardlink.txt")).expect("the link"),
        body,
        "the archive holds a link to real content, so the extraction does too"
    );
    assert_eq!(
        std::fs::read(dest.join("original.txt")).expect("the original"),
        body
    );
}

#[tokio::test]
async fn two_names_that_decode_alike_are_not_silently_one_member() {
    // Two *distinct* member names, both non-UTF-8, both decoding to the same
    // replacement characters. The index keyed on the decoded name and the
    // second entry replaced the first, so the panel was one row short, the
    // extraction was one file short, and nothing anywhere said so. The
    // collision is not even the container's: this program's own lossy decode
    // manufactured it.
    let tree = TempTree::new("collide");
    let container = tree.path("names.tar");
    craft_tar(
        &container,
        &[
            Craft::RawName(b"caf\xe9.txt", b"FIRST"),
            Craft::RawName(b"caf\xff.txt", b"SECOND"),
        ],
    );
    let session = session(&tree);
    let fs = session.open(&inside(&container, "/")).expect("open");
    fs.wait_for_index();

    let (refused, first) = fs.index().refusals();
    assert_eq!(refused, 1, "the displaced member is counted");
    let first = first.unwrap_or_default();
    assert!(
        first.contains("two entries share this name"),
        "and the reason says what happened: {first}"
    );

    // the rule, applied to a listing: what could not be shown is
    // never passed off as a complete answer.
    let (_, errors) = listing(fs.as_ref(), &inside(&container, "/")).await;
    assert!(
        errors.iter().any(|e| e.contains("refused")),
        "the listing reports it: {errors:?}"
    );
}

#[tokio::test]
async fn a_file_colliding_with_a_synthesised_directory_orphans_nothing() {
    // `a/b.txt` synthesises the directory `a`; a later regular member also
    // called `a` used to replace it. The children stayed in the index under a
    // path that was no longer a directory, so nothing ever walked them:
    // `a/b.txt` vanished from the listing and from every extraction, and the
    // job reported success.
    let tree = TempTree::new("orphan");
    let container = tree.path("shadow.tar");
    craft_tar(
        &container,
        &[
            Craft::File("a/b.txt", b"inside the directory", 0o644),
            Craft::File("a", b"not a directory", 0o644),
        ],
    );
    let session = session(&tree);
    let fs = session.open(&inside(&container, "/")).expect("open");
    fs.wait_for_index();

    let (refused, _) = fs.index().refusals();
    assert_eq!(refused, 1, "the colliding entry is refused, and counted");
    assert!(
        fs.index().is_dir("a"),
        "the directory its children live in stands"
    );
    let (rows, _) = listing(fs.as_ref(), &inside(&container, "/a")).await;
    assert!(
        rows.iter().any(|r| r.name == "b.txt"),
        "and the member below it is still reachable: {rows:?}"
    );
}

#[test]
fn an_unrelated_edit_keeps_a_non_utf8_member_name_byte_for_byte() {
    // A tar stores names as bytes with no declared encoding. The index decodes
    // them lossily because the panel draws text - and the rewrite used to
    // write that decoding *back*, so adding one unrelated file replaced a
    // Latin-1 byte with U+FFFD in the archive, permanently and silently. An
    // edit to one member must be an edit to one member.
    let tree = TempTree::new("names");
    let container = tree.path("latin.tar.gz");
    craft_tar_gz(&container, &[Craft::RawName(b"caf\xe9.txt", b"body")]);
    assert_eq!(raw_tar_gz_names(&container), vec![b"caf\xe9.txt".to_vec()]);

    let source = tree.path("added.txt");
    std::fs::write(&source, b"an unrelated addition").expect("write");
    FormatId::TarGz
        .backend()
        .apply(
            &container,
            &[MemberEdit::Put {
                member_path: "added.txt".to_string(),
                source,
                mode: 0o644,
                mtime: None,
            }],
            &mut NoProgress,
        )
        .expect("the rewrite");

    let names = raw_tar_gz_names(&container);
    assert!(
        names.contains(&b"caf\xe9.txt".to_vec()),
        "the surviving member keeps the bytes the container stored: {names:?}"
    );
    assert!(names.contains(&b"added.txt".to_vec()), "{names:?}");
}

#[test]
fn a_member_that_declares_a_bomb_is_refused_before_it_is_read() {
    // the decompression bomb. Charging bytes as they arrive stops
    // a member that lies about being small; it does not stop one that is
    // honest about being enormous, and the check that can - the declared size
    // against the container's own - was unreachable from anything the program
    // ran. Nothing may be read, reserved or written on the strength of this
    // number.
    let tree = TempTree::new("bomb");
    let container = tree.path("bomb.tar");
    craft_tar(
        &container,
        &[Craft::Bomb("huge.bin", 8 * 1024 * 1024 * 1024)],
    );
    let session = session(&tree);
    let fs = session.open(&inside(&container, "/")).expect("open");
    fs.wait_for_index();

    let err = fs
        .open_read(&inside(&container, "/huge.bin"))
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    assert!(
        err.contains("refused"),
        "refused with the rule it was refused under: {err}"
    );
}

/// An outer zip holding an inner zip, which is the nesting.
fn nested_zip(tree: &TempTree, inner_members: &[(&str, &[u8])]) -> PathBuf {
    let inner = tree.path("inner.zip");
    write_zip(&inner, inner_members);
    let bytes = std::fs::read(&inner).expect("read the inner archive");
    std::fs::remove_file(&inner).expect("tidy");
    let outer = tree.path("outer.zip");
    write_zip(&outer, &[("nest/inner.zip", &bytes)]);
    outer
}

/// `outer.zip#/nest/inner.zip#/<member>`.
fn inside_nested(outer: &Path, member: &str) -> VfsPath {
    VfsPath::local(outer)
        .with_segment(BackendKind::Archive, "/nest/inner.zip")
        .with_segment(BackendKind::Archive, member)
}

#[tokio::test]
async fn a_nested_archive_is_read_only_and_says_so_before_the_question() {
    // A nested archive is opened from a **copy** in the session cache.
    // A write to it landed on that temp file, changed nothing
    // in the archive the user was looking at, and reported success - and a
    // `Move` into one deleted the local source afterwards, leaving the only
    // copy of those bytes in a file the session removes on exit.
    let tree = TempTree::new("nested");
    let outer = nested_zip(&tree, &[("a.txt", b"first"), ("b.txt", b"second")]);
    let before = std::fs::read(&outer).expect("read");

    let session = session(&tree);
    let fs = session
        .open(&inside_nested(&outer, "/a.txt"))
        .expect("open the inner archive");
    assert!(fs.is_nested());
    assert!(
        !fs.capabilities().writable,
        "the UI consults this before offering the operation"
    );
    let err = fs
        .remove(&inside_nested(&outer, "/a.txt"))
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    assert!(
        err.contains("Extract it, change it, and put it back"),
        "refused, and the message says what to do instead: {err}"
    );

    // And through `ops`, which is where the data loss was: a move whose copy
    // went nowhere still deleted the source.
    let source = tree.path("precious.txt");
    std::fs::write(&source, b"the only copy").expect("write");
    let summary = run_job(
        router(&tree),
        JobSpec::new(
            JobKind::Move,
            vec![VfsPath::local(&source)],
            Some(inside_nested(&outer, "/")),
        ),
    )
    .await;
    assert!(
        !summary.is_clean(),
        "a write that cannot be delivered is refused, not reported as done"
    );
    assert!(source.exists(), "and the source is still there");
    assert_eq!(
        std::fs::read(&outer).expect("read"),
        before,
        "the archive the user is looking at is untouched either way"
    );
}

#[tokio::test]
async fn an_open_nested_archive_keeps_the_container_it_is_reading() {
    // The cache evicts an entry only when nothing outside it holds one - and
    // an open nested archive held only the *path*, so the eviction test read
    // its own reference count as "nobody is using this" and unlinked the
    // container of an archive a panel was standing inside. Every later read
    // through it failed with `ENOENT`.
    let tree = TempTree::new("evict");
    let outer = nested_zip(&tree, &[("deep.txt", b"still readable")]);
    let session = session(&tree);
    let fs = session
        .open(&inside_nested(&outer, "/deep.txt"))
        .expect("open the inner archive");
    let container = fs.container().to_path_buf();
    assert!(container.exists());

    // Force the cache to sweep for something to evict. Whether it finds room
    // is not the point; that it does not take *this* is.
    let _ = session.make_room_for_test(u64::MAX / 2);

    assert!(
        container.exists(),
        "the container of an open archive is not evicted out from under it"
    );
    let mut reader = fs
        .open_read(&inside_nested(&outer, "/deep.txt"))
        .expect("still readable");
    let mut got = Vec::new();
    reader.read_to_end(&mut got).expect("read");
    assert_eq!(got, b"still readable");
}

#[tokio::test]
async fn a_pack_writes_the_container_once_and_is_not_gated_as_a_rewrite() {
    // `Alt+F5` used to create an empty container and fill it with an ordinary
    // `F5`, one member per rewrite: quadratic in the member count, and gated
    // by the design *during* the job rather than before it, so a pack past
    // `rewrite_max_size` wrote part of itself and then refused every remaining
    // file - advising the user to repack the pack they had just asked for.
    // The whole selection is one `create` now, so neither can happen.
    let tree = TempTree::new("pack");
    let src = tree.path("src");
    std::fs::create_dir_all(&src).expect("mkdir");
    let body = vec![b'z'; 8 * 1024];
    let mut sources = Vec::new();
    for n in 0..8 {
        let at = src.join(format!("f{n}.bin"));
        std::fs::write(&at, &body).expect("write");
        sources.push(VfsPath::local(&at));
    }

    // A limit far below what the pack will write. the gates are about
    // rewriting an archive that already exists; a pack rewrites nothing.
    let base = tree.path("session");
    std::fs::create_dir_all(&base).expect("session base");
    let router = Arc::new(VfsRouter::new(
        ArchiveConfig {
            temp_dir: base.to_string_lossy().into_owned(),
            rewrite_warn_size: crate::config::ByteSize(1024),
            rewrite_max_size: crate::config::ByteSize(4096),
            ..ArchiveConfig::default()
        },
        crate::config::RemoteConfig::default(),
    ));

    let container = tree.path("out.tar.gz");
    let options = crate::ops::JobOptions {
        pack: Some(crate::ops::PackInto {
            format: FormatId::TarGz,
            level: 6,
        }),
        ..crate::ops::JobOptions::default()
    };
    let summary = run_job(
        router,
        JobSpec::new(JobKind::Copy, sources, Some(VfsPath::local(&container)))
            .with_options(options),
    )
    .await;

    assert!(summary.is_clean(), "{:?}", summary.failures);
    assert_eq!(summary.files_done, 8, "every file was packed");
    let names = raw_tar_gz_names(&container);
    assert_eq!(
        names.len(),
        8,
        "and the archive holds all of them: {names:?}"
    );
}

#[tokio::test]
async fn a_rar_symlink_never_copies_a_file_the_archive_does_not_hold() {
    // The `.rar` symlink escape, end to end through the engine `Alt+F6`
    // reaches. The archive contains no file called `victim.txt`; it contains a
    // member whose attributes say "symbolic link" and whose body is a relative
    // path with enough `..` in it to leave the session cache. `libunrar`
    // reproduces such a member by calling `symlink(2)` at the path it is
    // handed - and `IsRelativeSymlinkSafe` measures those `..` against the
    // depth of that path, which is a directory this program chose and not the
    // archive's root, so it passes. The extraction then read *through* the
    // link, and the destination came out holding the contents of a file that
    // was never in the archive, with an empty failure list.
    use super::rar::fixture::{Entry as RarEntry, SYMLINK_ATTR, write_rar};

    let tree = TempTree::new("rarlink");
    // The session's cache lives at `<tree>/session/hcmd-archive-<pid>-<n>/`,
    // and a materialised member at `<that>/member-<n>/<name>`. Two `..` from
    // there reach `<tree>/session`, which is where the victim is put: the
    // member's *name* is three components deep, so `libunrar`'s own check -
    // which measures the `..` against the name rather than against the
    // directory it is actually writing into - allows exactly this. The real
    // exploit uses the same two levels of slack against `$TMPDIR` and walks on
    // to `$HOME`.
    let base = tree.path("session");
    std::fs::create_dir_all(&base).expect("session base");
    let victim = base.join("victim.txt");
    std::fs::write(&victim, b"THE VICTIM FILE").expect("write");

    let container = tree.path("evil.rar");
    write_rar(
        &container,
        &[
            RarEntry::file("safe.txt", b"a legitimate member"),
            RarEntry {
                name: "x\\y\\link",
                data: b"../../victim.txt",
                dir: false,
                attr: SYMLINK_ATTR,
            },
        ],
    );

    let dest = tree.path("out");
    std::fs::create_dir_all(&dest).expect("mkdir");
    let summary = run_job(
        router(&tree),
        JobSpec::new(
            JobKind::Copy,
            vec![inside(&container, "/")],
            Some(VfsPath::local(&dest)),
        ),
    )
    .await;

    // The legitimate member came out. a refusal is per entry
    // and never aborts the batch.
    assert_eq!(
        std::fs::read(dest.join("safe.txt")).expect("safe.txt"),
        b"a legitimate member"
    );
    // And the link produced nothing at all - not a link, and above all not a
    // regular file holding somebody else's bytes.
    let planted = dest.join("x/y/link");
    match std::fs::read(&planted) {
        Err(_) => {}
        Ok(bytes) => panic!(
            "the destination holds {} bytes copied out of a file the archive \
             never contained",
            bytes.len()
        ),
    }
    assert!(
        !summary.is_clean(),
        "and the refusal is reported rather than swallowed"
    );
}

#[tokio::test]
async fn members_read_out_of_order_are_still_byte_exact() {
    // A compressed tar has no seek, so reading a member means decompressing
    // everything before it - and reading *every* member that way decompressed
    // the container once per member: `Alt+F6` on a 300-member `.tar.gz` cost
    // 150 times one sequential pass. A decoder that has just finished serving
    // a member is now kept and resumed by the next read that wants a later
    // offset (`tar::cursors`).
    //
    // Which makes this the test that matters: a resumed decoder is a *shared
    // position*, and getting it wrong hands a caller somebody else's bytes. So
    // every member is read forwards, backwards, and twice over, and every read
    // is compared with what went in.
    let tree = TempTree::new("order");
    let container = tree.path("many.tar.gz");
    let bodies: Vec<Vec<u8>> = (0..12u8)
        .map(|n| {
            (0..=255u8)
                .map(|b| b.wrapping_mul(n).wrapping_add(n))
                .cycle()
                .take(3000 + usize::from(n))
                .collect()
        })
        .collect();
    let names: Vec<String> = (0..12).map(|n| format!("m{n:02}.bin")).collect();
    let members: Vec<(&str, &[u8])> = names
        .iter()
        .zip(bodies.iter())
        .map(|(n, b)| (n.as_str(), b.as_slice()))
        .collect();
    write_tar_gz(&container, &members);

    let session = session(&tree);
    let fs = session.open(&inside(&container, "/")).expect("open");
    fs.wait_for_index();

    let read_one = |n: usize| -> Vec<u8> {
        let mut reader = fs
            .open_read(&inside(&container, &format!("/{}", names[n])))
            .expect("open the member");
        let mut got = Vec::new();
        reader.read_to_end(&mut got).expect("read");
        got
    };

    let forwards: Vec<usize> = (0..12).collect();
    let backwards: Vec<usize> = (0..12).rev().collect();
    let mixed = vec![7usize, 0, 11, 3, 3, 9, 1, 11];
    for order in [forwards, backwards, mixed] {
        for n in order {
            assert_eq!(read_one(n), bodies[n], "member {n} read out of order");
        }
    }
}

#[tokio::test]
async fn a_read_only_archive_refuses_a_delete_once_rather_than_per_file() {
    // The guard asked the `Vfs` it was handed - which is the router, and a
    // router's own `capabilities` can only answer for the local filesystem.
    // So `Shift+F8` inside a `.rar` walked straight past it and
    // produced one failure row per marked file, from `Vfs::remove`, instead of
    // one refusal naming the backend.
    use super::rar::fixture::{Entry as RarEntry, write_rar};

    let tree = TempTree::new("rodelete");
    let container = tree.path("book.rar");
    write_rar(
        &container,
        &[
            RarEntry::file("a.txt", b"one"),
            RarEntry::file("b.txt", b"two"),
            RarEntry::file("c.txt", b"three"),
        ],
    );

    let summary = run_job(
        router(&tree),
        JobSpec::new(
            JobKind::Delete { trash: false },
            vec![
                inside(&container, "/a.txt"),
                inside(&container, "/b.txt"),
                inside(&container, "/c.txt"),
            ],
            None,
        ),
    )
    .await;

    assert_eq!(
        summary.failures.len(),
        3,
        "one refusal per source named, and no work attempted: {:?}",
        summary.failures
    );
    assert!(
        summary
            .failures
            .iter()
            .all(|f| f.error.contains("read-only")),
        "the reason is the backend, not three separate `remove` errors: {:?}",
        summary.failures
    );
    assert_eq!(summary.files_done, 0);
    // Nothing was touched: the container is still exactly what it was.
    assert!(container.exists());
}

#[tokio::test]
async fn an_archive_job_reports_its_totals_before_it_starts() {
    // "Counts are `done / total`, files and bytes both."
    // `run_through_vfs` never called `ctx.start`, so every archive job ran
    // with `files_total = 0` and `bytes_total = 0`: no batch bar, no
    // percentage, no ETA - and, since `JobStatus` only becomes `Running` when
    // a `Started` event arrives, a queue view that said `Pending` for the
    // whole run.
    let tree = TempTree::new("totals");
    let container = tree.path("bundle.tar.gz");
    write_tar_gz(
        &container,
        &[
            ("hello.txt", b"hello"),
            ("sub/deep.txt", b"deeper"),
            ("sub/deeper.txt", b"deepest"),
        ],
    );
    let dest = tree.path("out");
    std::fs::create_dir_all(&dest).expect("mkdir");

    let router = router(&tree);
    let spec = JobSpec::new(
        JobKind::Copy,
        vec![inside(&container, "/")],
        Some(VfsPath::local(&dest)),
    );
    let started = tokio::task::spawn_blocking(move || {
        let (mut ctx, mut rx, dtx, _flag) = JobContext::for_test(spec.kind);
        drop(dtx);
        // Drained on another thread: the update channel is bounded, and a job
        // nobody is reading would block on it.
        let collector = std::thread::spawn(move || {
            let mut started = None;
            while let Some(update) = rx.blocking_recv() {
                if let crate::ops::JobEvent::Started {
                    files_total,
                    bytes_total,
                    ..
                } = update.event
                {
                    started = Some((files_total, bytes_total));
                }
            }
            started
        });
        crate::ops::run(router.as_ref(), &spec, &mut ctx);
        let _ = ctx.finish();
        collector.join().expect("the collector")
    })
    .await
    .expect("the job thread");

    let (files, bytes) = started.expect("the job says what its total is");
    assert_eq!(files, 3, "every member counted before a byte moved");
    assert_eq!(bytes, 5 + 6 + 7, "and their bytes too");
}
