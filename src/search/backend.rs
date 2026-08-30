//! Searching what `ignore` cannot walk (the last bullet).
//!
//! > Archives and remote panels are searched through `Vfs`, not through
//! > `ignore`, which only walks the local filesystem.
//!
//! Two callers reach this: a search root that is already inside an archive, and
//! the "Search archives", which opens every archive the local walk
//! passed and walks its members here. Every hit carries a real
//! [`crate::vfs::Entry::location`] - `/a/b.tar.gz#/inner/c.txt` - so `F3`, `F5`
//! and `F8` reach it unchanged.
//!
//! # Breadth-first over an explicit queue, never recursive
//!
//! A tree deep enough to overflow the stack is one malformed archive away, and
//! this runs on a worker thread with a worker thread's stack. The queue is
//! bounded at [`MAX_QUEUE`]; past that, directories are searched but not
//! descended, and the [`Tally`] says the walk was cut short rather than
//! pretending it was complete.
//!
//! # Content search here cannot be memory-mapped and does not try
//!
//! A member is read through [`crate::vfs::Vfs::open_read`], once per selected
//! charset, because a `Vfs` stream cannot be rewound. That is the cost of
//! searching inside an archive and it is why the design makes it a
//! checkbox.

use std::collections::VecDeque;
use std::time::SystemTime;

use crate::vfs::list::ListSink;
use crate::vfs::{BackendKind, Entry, Vfs, VfsPath};

use super::content::Outcome;
use super::query::Compiled;
use super::walk::Tally;

/// The most directories the `Vfs` walk queues at once.
///
/// A hundred thousand pending directories is far past any real tree and far
/// below anything that could exhaust memory on its own.
pub const MAX_QUEUE: usize = 100_000;

/// Walk a root that `ignore` cannot walk.
///
/// Used for a root inside an archive, and for an archive entered under
/// "Search archives". **Blocking**; it drives [`crate::vfs::Vfs::read_dir`]'s
/// receiver with `blocking_recv`, exactly as `ops::walk` does, so it belongs on
/// the blocking pool and never on a runtime worker.
pub fn walk(vfs: &dyn Vfs, root: &VfsPath, compiled: &Compiled, sink: &ListSink) -> Tally {
    let mut tally = Tally::default();
    // One instant for the whole walk, so a date filter means one thing.
    let now = SystemTime::now();
    let max_depth = compiled.query().depth.max_depth();
    let mut queue: VecDeque<(VfsPath, usize)> = VecDeque::new();
    queue.push_back((root.clone(), 0));

    while let Some((dir, depth)) = queue.pop_front() {
        if sink.is_cancelled() {
            return tally;
        }
        let mut rx = vfs.read_dir(&dir);
        let mut failed = false;
        while let Some(item) = rx.blocking_recv() {
            if sink.is_cancelled() {
                // Dropping the receiver is what stops the producing task, the
                // same way every other cancelled listing stops.
                return tally;
            }
            let entry = match item {
                Ok(entry) => entry,
                Err(_) => {
                    failed = true;
                    continue;
                }
            };
            if entry.is_parent {
                continue;
            }
            tally.visited = tally.visited.saturating_add(1);
            let child_depth = depth.saturating_add(1);
            // A row's own address if it has one, and the path it was listed
            // under otherwise. An archive listing fills `location` in for a
            // member; a directory listing does not, because the two are the
            // same path.
            let child = entry
                .location
                .clone()
                .unwrap_or_else(|| dir.join(&entry.name));

            // Descend only when what is *inside* would still be admitted, so
            // `none` reads the root once rather than reading every
            // subdirectory and discarding it.
            let descend = admits(max_depth, child_depth.saturating_add(1));
            if entry.is_dir() && descend {
                enqueue(&mut queue, (child.clone(), child_depth), &mut tally);
            } else if !entry.is_dir()
                && descend
                && compiled.query().search_archives
                && looks_like_archive(&entry.name)
            {
                // "Archives are directories": entering one is a
                // segment on the path, so a nested archive needs no special
                // case here either.
                let inside = child.clone().with_segment(BackendKind::Archive, "/");
                enqueue(&mut queue, (inside, child_depth), &mut tally);
            }

            if !admits(max_depth, child_depth) {
                continue;
            }
            if !compiled.name_matches(&entry.name) {
                continue;
            }
            if !compiled.attrs_match(&entry, now) {
                continue;
            }
            let hit = match content_of(vfs, compiled, &entry, &child, &mut tally) {
                Some(hit) => hit,
                None => continue,
            };

            let mut row = entry;
            row.location = Some(child);
            row.hit = hit;
            tally.matched = tally.matched.saturating_add(1);
            if !sink.push(row) {
                return tally;
            }
        }
        if failed {
            tally.unreadable = tally.unreadable.saturating_add(1);
            if tally.first_problem.is_none() {
                tally.first_problem = Some(format!("{dir} could not be read in full"));
            }
        }
    }
    tally
}

/// Is an entry at this depth inside the dropdown's answer?
///
/// `max_depth` is `ignore`'s: the root is 0 and its own entries are 1, which is
/// what keeps the two walks agreeing about what `1 level` means.
const fn admits(max_depth: Option<usize>, depth: usize) -> bool {
    match max_depth {
        None => true,
        Some(max) => depth <= max,
    }
}

/// Queue a directory, or count it as a place the walk did not reach.
fn enqueue(queue: &mut VecDeque<(VfsPath, usize)>, item: (VfsPath, usize), tally: &mut Tally) {
    if queue.len() >= MAX_QUEUE {
        tally.unreadable = tally.unreadable.saturating_add(1);
        if tally.first_problem.is_none() {
            tally.first_problem = Some(format!(
                "the walk queue reached {MAX_QUEUE} directories and stopped descending"
            ));
        }
        return;
    }
    queue.push_back(item);
}

/// The content answer for one member: `Some(hit)` when it qualifies.
///
/// `Some(None)` is a qualifying row with no hit to point at, which is a
/// name-only search and an inverted content search alike.
fn content_of(
    vfs: &dyn Vfs,
    compiled: &Compiled,
    entry: &Entry,
    path: &VfsPath,
    tally: &mut Tally,
) -> Option<Option<Box<crate::vfs::ContentHit>>> {
    let Some(content) = compiled.content() else {
        return Some(None);
    };
    // A directory has no contents to search, so it cannot contain the text and
    // cannot be shown to be missing it either.
    if entry.is_dir() {
        return None;
    }
    if entry.size > content.max_bytes() {
        tally.skipped_large = tally.skipped_large.saturating_add(1);
        return None;
    }

    let mut binary = false;
    for charset in content.charsets() {
        // Once per charset, because a `Vfs` stream cannot be rewound.
        let mut reader = match vfs.open_read(path) {
            Ok(reader) => reader,
            Err(err) => {
                tally.unreadable = tally.unreadable.saturating_add(1);
                if tally.first_problem.is_none() {
                    tally.first_problem = Some(format!("{path}: {err}"));
                }
                return None;
            }
        };
        match content.search_charset(&mut reader, *charset, &path.to_string()) {
            Outcome::Match(hit) => {
                return if content.inverted() { None } else { Some(hit) };
            }
            Outcome::NoMatch => {}
            Outcome::Binary => {
                binary = true;
                break;
            }
            Outcome::Skipped(why) => {
                tally.unreadable = tally.unreadable.saturating_add(1);
                if tally.first_problem.is_none() {
                    tally.first_problem = Some(why);
                }
                return None;
            }
        }
    }
    if binary {
        // Absence cannot be concluded from a file the searcher refused to read
        // to the end, so an inverted search does not claim it either.
        tally.skipped_binary = tally.skipped_binary.saturating_add(1);
        return None;
    }
    if content.inverted() { Some(None) } else { None }
}

/// Is this name worth opening as an archive under "Search archives"?
///
/// **By extension only**, deliberately: the design detects "by content
/// sniffing first, extension second", which costs one open and one read per
/// candidate. That is right for the one file `Enter` was pressed on and wrong
/// for every file in a tree.
///
/// The extension language is [`crate::vfs::archive::format::FormatId`]'s, so
/// `Alt+F5`, `Enter` and this cannot disagree about what a `.tgz` is.
pub fn looks_like_archive(name: &str) -> bool {
    crate::vfs::archive::format::FormatId::from_name(name).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::query::{ContentQuery, Depth, Query};
    use crate::vfs::list::{ListFs, ListStatus};
    use crate::vfs::{Capabilities, EntryKind};
    use std::collections::HashMap;
    use std::sync::Mutex;
    use tokio::sync::mpsc;

    /// A `Vfs` made of a map, so a walk can be driven with no filesystem at
    /// all - including one deep enough to overflow a stack.
    #[derive(Debug, Default)]
    struct FakeFs {
        /// Directory path -> its entries.
        dirs: HashMap<String, Vec<Entry>>,
        /// File path -> its contents.
        files: HashMap<String, Vec<u8>>,
        /// Every path `open_read` was asked for, in order.
        reads: Mutex<Vec<String>>,
        /// A listing to cancel as the `n`th directory is opened, and the
        /// count so far.
        ///
        /// This is how a cancel is delivered *during* a walk rather than
        /// before it: the flag goes up inside the call the walk is making,
        /// with directories still queued behind it.
        cancel_at: Option<(usize, std::sync::Arc<ListFs>)>,
        /// How many directories have been opened.
        opened: std::sync::atomic::AtomicUsize,
    }

    impl FakeFs {
        /// Cancel `listing` as the `nth` directory of the walk is opened.
        fn cancel_when_opening_directory(&mut self, nth: usize, listing: std::sync::Arc<ListFs>) {
            self.cancel_at = Some((nth, listing));
        }

        fn dir(&mut self, path: &str, names: &[(&str, bool)]) {
            let entries = names
                .iter()
                .map(|(name, is_dir)| {
                    if *is_dir {
                        Entry::dir(*name)
                    } else {
                        Entry::file(*name)
                    }
                })
                .collect();
            self.dirs.insert(path.to_string(), entries);
        }

        fn file(&mut self, path: &str, body: &str) {
            self.files
                .insert(path.to_string(), body.as_bytes().to_vec());
        }
    }

    impl Vfs for FakeFs {
        fn kind(&self) -> BackendKind {
            BackendKind::Archive
        }

        fn read_dir(&self, path: &VfsPath) -> mpsc::Receiver<crate::error::Result<Entry>> {
            if let Some((nth, listing)) = self.cancel_at.as_ref() {
                let opened = self
                    .opened
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                    .saturating_add(1);
                if opened >= *nth {
                    listing.cancel();
                }
            }
            let (tx, rx) = mpsc::channel(64);
            let entries = self.dirs.get(&path.to_string()).cloned();
            tokio::spawn(async move {
                match entries {
                    Some(entries) => {
                        for entry in entries {
                            if tx.send(Ok(entry)).await.is_err() {
                                return;
                            }
                        }
                    }
                    None => {
                        let _ = tx
                            .send(Err(crate::error::Error::NotFound("no such dir".into())))
                            .await;
                    }
                }
            });
            rx
        }

        fn stat(&self, path: &VfsPath) -> crate::error::Result<Entry> {
            Ok(Entry::file(path.file_name().unwrap_or_default()))
        }

        fn open_read(&self, path: &VfsPath) -> crate::error::Result<Box<dyn std::io::Read + Send>> {
            let key = path.to_string();
            if let Ok(mut reads) = self.reads.lock() {
                reads.push(key.clone());
            }
            match self.files.get(&key) {
                Some(body) => Ok(Box::new(std::io::Cursor::new(body.clone()))),
                None => Err(crate::error::Error::NotFound(key)),
            }
        }

        fn open_write(
            &self,
            _path: &VfsPath,
        ) -> crate::error::Result<Box<dyn std::io::Write + Send>> {
            Err(crate::error::Error::Unsupported("write"))
        }

        fn create_dir(&self, _path: &VfsPath) -> crate::error::Result<()> {
            Err(crate::error::Error::Unsupported("mkdir"))
        }

        fn remove(&self, _path: &VfsPath) -> crate::error::Result<()> {
            Err(crate::error::Error::Unsupported("remove"))
        }

        fn rename(&self, _from: &VfsPath, _to: &VfsPath) -> crate::error::Result<()> {
            Err(crate::error::Error::Unsupported("rename"))
        }

        fn capabilities(&self) -> Capabilities {
            Capabilities::ARCHIVE_UNKNOWN
        }
    }

    fn root() -> VfsPath {
        VfsPath::local("/a/b.zip").with_segment(BackendKind::Archive, "/")
    }

    fn listing() -> (std::sync::Arc<ListFs>, ListSink) {
        ListFs::streaming("test", &[root()])
    }

    fn names(listing: &ListFs) -> Vec<String> {
        let (_, rows, _) = listing.snapshot_from(0);
        let mut names: Vec<String> = rows.iter().map(|e| e.name.clone()).collect();
        names.sort();
        names
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_member_of_an_archive_is_a_row_with_a_real_address() {
        let mut fs = FakeFs::default();
        fs.dir("/a/b.zip#/", &[("inner", true), ("top.txt", false)]);
        fs.dir("/a/b.zip#/inner", &[("c.rs", false)]);

        let mut q = Query::new(root());
        q.name = "*.rs".to_string();
        let compiled = q.compile().expect("compiles");
        let (list, sink) = listing();
        let tally = tokio::task::spawn_blocking(move || {
            let tally = walk(&fs, &root(), &compiled, &sink);
            sink.finish(ListStatus::Complete);
            tally
        })
        .await
        .expect("the walk finished");

        assert_eq!(tally.matched, 1);
        let (_, rows, _) = list.snapshot_from(0);
        let row = rows.first().expect("one row");
        assert_eq!(row.name, "c.rs");
        assert_eq!(
            row.location.as_ref().map(ToString::to_string),
            Some("/a/b.zip#/inner/c.rs".to_string()),
            "every operation on a result addresses the real member"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_thousand_levels_deep_is_walked_and_not_recursed() {
        // The reason this walk is a queue: a tree deep enough to overflow the
        // stack is one malformed archive away.
        let mut fs = FakeFs::default();
        let mut path = root();
        for _ in 0..1_000 {
            fs.dir(&path.to_string(), &[("d", true)]);
            path = path.join("d");
        }
        fs.dir(&path.to_string(), &[("bottom.txt", false)]);

        let mut q = Query::new(root());
        q.name = "bottom.txt".to_string();
        let compiled = q.compile().expect("compiles");
        let (list, sink) = listing();
        tokio::task::spawn_blocking(move || {
            walk(&fs, &root(), &compiled, &sink);
            sink.finish(ListStatus::Complete);
        })
        .await
        .expect("the walk finished");
        assert_eq!(names(&list), vec!["bottom.txt"]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_content_search_reads_the_member_once_per_charset() {
        let mut fs = FakeFs::default();
        fs.dir("/a/b.zip#/", &[("a.txt", false), ("b.txt", false)]);
        fs.file("/a/b.zip#/a.txt", "nothing here\n");
        fs.file("/a/b.zip#/b.txt", "a TODO line\n");

        let mut q = Query::new(root());
        q.content = Some(ContentQuery {
            pattern: "TODO".to_string(),
            ..ContentQuery::default()
        });
        let compiled = q.compile().expect("compiles");
        let (list, sink) = listing();
        let fs = tokio::task::spawn_blocking(move || {
            walk(&fs, &root(), &compiled, &sink);
            sink.finish(ListStatus::Complete);
            fs
        })
        .await
        .expect("the walk finished");

        assert_eq!(names(&list), vec!["b.txt"]);
        let reads = fs.reads.lock().expect("lock");
        assert_eq!(reads.len(), 2, "one open per member per charset: {reads:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_inverted_search_inside_an_archive_reports_the_others() {
        let mut fs = FakeFs::default();
        fs.dir("/a/b.zip#/", &[("a.txt", false), ("b.txt", false)]);
        fs.file("/a/b.zip#/a.txt", "nothing here\n");
        fs.file("/a/b.zip#/b.txt", "a TODO line\n");

        let mut q = Query::new(root());
        q.content = Some(ContentQuery {
            pattern: "TODO".to_string(),
            inverted: true,
            ..ContentQuery::default()
        });
        let compiled = q.compile().expect("compiles");
        let (list, sink) = listing();
        tokio::task::spawn_blocking(move || {
            walk(&fs, &root(), &compiled, &sink);
            sink.finish(ListStatus::Complete);
        })
        .await
        .expect("the walk finished");
        assert_eq!(names(&list), vec!["a.txt"]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelling_before_the_walk_starts_finds_nothing() {
        let mut fs = FakeFs::default();
        fs.dir("/a/b.zip#/", &[("a.txt", false)]);
        let compiled = Query::new(root()).compile().expect("compiles");
        let (list, sink) = listing();
        sink.cancel();
        let tally = tokio::task::spawn_blocking(move || walk(&fs, &root(), &compiled, &sink))
            .await
            .expect("the walk finished");
        assert_eq!(tally.matched, 0);
        assert!(list.is_empty());
        assert_eq!(list.status(), ListStatus::Cancelled);
    }

    /// The half the name of the test above cannot cover: `Esc` is pressed
    /// while the walk is running, and it has to stop where it is and keep what
    /// it had.
    ///
    /// A walk that reads the flag once on the way in and never again passes
    /// that test and keeps reading a whole tree after the panel has moved on,
    /// so here the flag goes up as the second directory is opened, with ten
    /// more still queued behind it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelling_mid_walk_stops_it_and_keeps_what_was_found() {
        const DIRS: u64 = 10;
        const PER_DIR: u64 = 20;
        let mut fs = FakeFs::default();

        let mut top: Vec<(String, bool)> = (0..DIRS).map(|i| (format!("d{i}"), true)).collect();
        top.push(("hit.txt".to_string(), false));
        let rows: Vec<(&str, bool)> = top.iter().map(|(n, d)| (n.as_str(), *d)).collect();
        fs.dir("/a/b.zip#/", &rows);
        for i in 0..DIRS {
            let inside: Vec<String> = (0..PER_DIR).map(|n| format!("d{i}f{n:02}.txt")).collect();
            let rows: Vec<(&str, bool)> = inside.iter().map(|n| (n.as_str(), false)).collect();
            fs.dir(&format!("/a/b.zip#/d{i}"), &rows);
        }

        let (list, sink) = listing();
        fs.cancel_when_opening_directory(2, std::sync::Arc::clone(&list));
        let compiled = Query::new(root()).compile().expect("compiles");
        let tally = tokio::task::spawn_blocking(move || walk(&fs, &root(), &compiled, &sink))
            .await
            .expect("the walk finished");

        assert_eq!(list.status(), ListStatus::Cancelled);
        let found = names(&list);
        assert!(
            found.contains(&"hit.txt".to_string()),
            "what was found before the cancel is kept: {found:?}"
        );
        assert!(
            !found.iter().any(|n| n.starts_with("d0f")),
            "it carried on into the directory it was cancelled in: {found:?}"
        );
        assert_eq!(
            tally.visited,
            DIRS.saturating_add(1),
            "only the root was ever read, not the {DIRS} directories under it"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_directory_that_cannot_be_read_is_counted_and_not_fatal() {
        let mut fs = FakeFs::default();
        fs.dir("/a/b.zip#/", &[("good", true), ("bad", true)]);
        fs.dir("/a/b.zip#/good", &[("found.txt", false)]);
        // `/a/b.zip#/bad` is missing from the map, so listing it fails.

        let compiled = Query::new(root()).compile().expect("compiles");
        let (list, sink) = listing();
        let tally = tokio::task::spawn_blocking(move || {
            let tally = walk(&fs, &root(), &compiled, &sink);
            sink.finish(ListStatus::Complete);
            tally
        })
        .await
        .expect("the walk finished");

        assert_eq!(tally.unreadable, 1);
        assert!(names(&list).contains(&"found.txt".to_string()));
        let note = tally.note(true).expect("a note");
        assert!(note.contains("1 unreadable"), "{note}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_depth_dropdown_means_the_same_thing_in_both_walks() {
        let mut fs = FakeFs::default();
        fs.dir("/a/b.zip#/", &[("one", true), ("top.txt", false)]);
        fs.dir("/a/b.zip#/one", &[("mid.txt", false), ("two", true)]);
        fs.dir("/a/b.zip#/one/two", &[("deep.txt", false)]);

        let mut q = Query::new(root());
        q.depth = Depth::None;
        let compiled = q.compile().expect("compiles");
        let (list, sink) = listing();
        let fs = tokio::task::spawn_blocking(move || {
            walk(&fs, &root(), &compiled, &sink);
            sink.finish(ListStatus::Complete);
            fs
        })
        .await
        .expect("the walk finished");
        assert_eq!(names(&list), vec!["one", "top.txt"]);

        let mut q = Query::new(root());
        q.depth = Depth::Levels(1);
        let compiled = q.compile().expect("compiles");
        let (list, sink) = listing();
        tokio::task::spawn_blocking(move || {
            walk(&fs, &root(), &compiled, &sink);
            sink.finish(ListStatus::Complete);
        })
        .await
        .expect("the walk finished");
        let found = names(&list);
        assert!(found.contains(&"mid.txt".to_string()), "{found:?}");
        assert!(!found.contains(&"deep.txt".to_string()), "{found:?}");
    }

    #[test]
    fn an_archive_is_recognised_by_the_one_extension_language() {
        for name in ["a.zip", "b.tar.gz", "c.tgz", "d.7z", "e.rar", "f.tar.zst"] {
            assert!(looks_like_archive(name), "{name}");
        }
        for name in ["notes.txt", "zip", "a.zipper", "b.tar.gzip"] {
            assert!(!looks_like_archive(name), "{name}");
        }
    }

    #[test]
    fn the_queue_is_bounded_and_says_so_rather_than_pretending() {
        let mut queue = VecDeque::new();
        let mut tally = Tally::default();
        for i in 0..MAX_QUEUE {
            enqueue(&mut queue, (VfsPath::local(format!("/{i}")), 1), &mut tally);
        }
        assert_eq!(queue.len(), MAX_QUEUE);
        assert_eq!(tally.unreadable, 0);
        enqueue(&mut queue, (VfsPath::local("/one-too-many"), 1), &mut tally);
        assert_eq!(queue.len(), MAX_QUEUE, "the queue did not grow");
        assert_eq!(tally.unreadable, 1, "and the walk admits it");
    }

    #[test]
    fn an_entry_kind_that_is_not_a_directory_is_never_descended() {
        // A guard on the one line that decides whether to queue: `is_dir` is
        // `Entry`'s, so a symlink to a directory inside an archive follows the
        // same rule the panel's `Enter` does.
        let mut link = Entry::file("l");
        link.kind = EntryKind::Symlink { to_dir: true };
        assert!(link.is_dir());
        let mut plain = Entry::file("p");
        plain.kind = EntryKind::Other;
        assert!(!plain.is_dir());
    }
}
