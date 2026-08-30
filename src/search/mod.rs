//! Search.
//!
//! > Search runs **in-process, using ripgrep's own libraries** - `ignore` for
//! > the walk, `grep-searcher` for the search, `grep-regex` for matching. This
//! > is not reimplementing ripgrep; it is calling it as a library instead of
//! > parsing its stdout, which gives the same engine and the same speed with
//! > structured results, no subprocess, and no external tool to be missing.
//!
//! Four files, and the split is by what each one talks to:
//!
//! * [`query`] - what a search *is*, and the one place a search is refused.
//!   Talks to nothing.
//! * [`walk`] - the `ignore` half. The only `WalkBuilder` in the crate.
//! * [`content`] - the `grep-searcher` half. The only searcher of a file's
//!   bytes.
//! * [`backend`] - what `ignore` cannot walk: archives, and later remote
//!   panels (the last bullet).
//!
//! # The results channel is the directory-read channel
//!
//! the design asks that "results stream back over a channel as they are
//! found, with a live count", and the design puts the results in the panel
//! that was active when the search started. Both are one mechanism:
//!
//! ```text
//! spawn ─▶ ListSink::push ─▶ ListFs ─▶ Vfs::read_dir ─▶ the panel
//! ```
//!
//! The walk fills a [`crate::vfs::list::ListFs`] through its
//! [`crate::vfs::list::ListSink`]; the panel is pointed at that listing and
//! reads it the way it reads a directory. Nothing here knows about panels,
//! frames or events, and nothing in the event loop knows about `ignore`.
//!
//! # Cancellation is the sink's flag and nothing else
//!
//! `Esc` cancels the listing; the walker's visitor answers `WalkState::Quit` at
//! its next entry, the `Vfs` walk checks it at its next directory and at its
//! next member, and [`crate::vfs::list::ListSink::push`] answers `false` so a
//! producer that checks nothing else still stops. `ops::CancelFlag` is
//! deliberately not used: a second flag with the same meaning is how one of
//! them gets forgotten.
//!
//! One consequence, stated rather than hidden: a **single** very large file
//! being content-searched is not interrupted part way. Cancellation is checked
//! between entries, so `Esc` is answered at the next file rather than at the
//! next byte.

pub mod backend;
pub mod content;
pub mod query;
pub mod saved;
pub mod state;
pub mod walk;

use std::sync::Arc;

pub use query::{
    AttrFilter, Charset, Charsets, Compiled, ContentQuery, DateRange, Depth, NameMode, Query,
    SizeRange, TextMode, Tri,
};
pub use state::Session;
pub use walk::Tally;

use crate::vfs::list::{ListSink, ListStatus};
use crate::vfs::{BackendKind, Vfs, VfsPath};

/// Options that are not part of the query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchOptions {
    /// Walker threads.
    ///
    /// Defaults to `available_parallelism` clamped to [`walk::MAX_THREADS`]. A
    /// test pins it to 1 to make the result order deterministic - a parallel
    /// walk is nondeterministic by construction, and nothing in the product
    /// depends on the order because the panel sorts what it is given.
    pub threads: usize,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            threads: std::thread::available_parallelism()
                .map(std::num::NonZeroUsize::get)
                .unwrap_or(1)
                .clamp(1, walk::MAX_THREADS),
        }
    }
}

/// Run a whole search to completion on this thread, filling `sink`.
///
/// Splits the roots: local ones go to [`walk::run`], everything else to
/// [`backend::walk`], because `ignore` "only walks the local filesystem".
/// Under the "Search archives" the archives the
/// local walk passed are walked afterwards, through the `Vfs`, so their hits
/// arrive after the local ones rather than not at all.
///
/// Returns the combined tally. **Blocking.**
pub fn run(vfs: &dyn Vfs, compiled: &Compiled, options: SearchOptions, sink: &ListSink) -> Tally {
    let mut tally = Tally::default();
    let threads = options.threads.clamp(1, walk::MAX_THREADS);

    let (local, archives) = walk::run_collecting_archives(compiled, threads, sink);
    tally.add(&local);

    for root in &compiled.query().roots {
        if sink.is_cancelled() {
            return tally;
        }
        // A local root was `ignore`'s; everything else is the `Vfs`'s.
        if root.backend() == BackendKind::Local && root.local_path().is_some() {
            continue;
        }
        tally.add(&backend::walk(vfs, root, compiled, sink));
    }

    for archive in archives {
        if sink.is_cancelled() {
            return tally;
        }
        // "Archives are directories": the archive's own path with
        // an `Archive` segment pushed is the address of its root.
        let inside = VfsPath::local(archive).with_segment(BackendKind::Archive, "/");
        tally.add(&backend::walk(vfs, &inside, compiled, sink));
    }
    tally
}

/// [`run`] on the blocking pool, closing the sink when it ends.
///
/// The exact analogue of `main::spawn_read` and `ops::spawn`: the caller keeps
/// the `Arc<ListFs>`, the worker keeps the sink, and cancelling the sink stops
/// the walk. The listing is closed here, once, whatever the walk did - a
/// listing that is never closed is a panel that says `searching` for ever.
///
/// Must be called from inside a tokio runtime.
///
/// The handle is returned rather than dropped so that the [`Tally`] can be
/// reached: the honesty rule wants one line at the end saying what
/// the walk passed over, and a caller that drops this handle is choosing not
/// to say it. Dropping it detaches the task, which is what every existing call
/// site does, so ignoring it costs nothing.
pub fn spawn(
    vfs: Arc<dyn Vfs>,
    compiled: Arc<Compiled>,
    options: SearchOptions,
    sink: ListSink,
) -> tokio::task::JoinHandle<Tally> {
    tokio::task::spawn_blocking(move || {
        let tally = run(vfs.as_ref(), compiled.as_ref(), options, &sink);
        // `finish` after a `cancel` leaves the status `Cancelled`, because the
        // user's answer outranks the producer's (`Esc` stops the
        // walk and keeps what was found).
        sink.finish(ListStatus::Complete);
        tally
    })
}

/// the panel header for one query, which is also the listing's
/// label: `[search: *.rs "TODO" in ~/dev]`.
///
/// Here as well as on [`Query`] so that a caller holding either can ask.
pub fn header(query: &Query) -> String {
    query.header()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::list::ListFs;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TempTree {
        root: PathBuf,
    }

    impl TempTree {
        fn new(tag: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or(Duration::ZERO)
                .as_nanos();
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "hcmd-searchrun-{tag}-{pid}-{nanos}-{n}",
                pid = std::process::id()
            ));
            fs::create_dir_all(&root).expect("create temp tree");
            Self { root }
        }

        fn file(&self, rel: &str, body: &str) {
            let path = self.root.join(rel);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("create parent");
            }
            fs::write(&path, body).expect("write");
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn the_default_thread_count_is_bounded() {
        let options = SearchOptions::default();
        assert!(options.threads >= 1);
        assert!(options.threads <= walk::MAX_THREADS);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_search_fills_the_listing_and_closes_it() {
        let t = TempTree::new("spawn");
        t.file("a.rs", "one\n");
        t.file("sub/b.rs", "two\n");
        t.file("sub/c.txt", "three\n");

        let mut query = Query::new(VfsPath::local(&t.root));
        query.name = "*.rs".to_string();
        let compiled = Arc::new(query.compile().expect("compiles"));
        let (listing, sink) = ListFs::streaming(query.header(), &query.roots);
        let vfs: Arc<dyn Vfs> = Arc::new(crate::vfs::LocalFs::new());

        let walk = spawn(vfs, compiled, SearchOptions { threads: 1 }, sink);
        // The listing is the panel's only view of the walk, so waiting on its
        // status is exactly what the panel does.
        let mut watch = listing.subscribe();
        while !listing.status().is_final() {
            let _ = watch.changed().await;
        }
        assert_eq!(listing.status(), ListStatus::Complete);
        let (_, rows, _) = listing.snapshot_from(0);
        let mut names: Vec<String> = rows.iter().map(|e| e.name.clone()).collect();
        names.sort();
        assert_eq!(names, vec!["a.rs", "b.rs"]);

        // And the tally is reachable rather than dropped, so the end-of-walk
        // line the design asks for has something to say.
        let tally = walk.await.expect("the walk finished");
        assert_eq!(tally.matched, 2);
        assert_eq!(tally.note(true), None, "nothing was passed over");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_cancelled_search_closes_as_cancelled_and_keeps_its_rows() {
        let t = TempTree::new("cancel");
        for i in 0..32 {
            t.file(&format!("f{i:03}.txt"), "x");
        }
        let query = Query::new(VfsPath::local(&t.root));
        let compiled = Arc::new(query.compile().expect("compiles"));
        let (listing, sink) = ListFs::streaming(query.header(), &query.roots);
        let vfs: Arc<dyn Vfs> = Arc::new(crate::vfs::LocalFs::new());

        listing.cancel();
        let _walk = spawn(vfs, compiled, SearchOptions { threads: 1 }, sink);
        let mut watch = listing.subscribe();
        while !listing.status().is_final() {
            let _ = watch.changed().await;
        }
        assert_eq!(
            listing.status(),
            ListStatus::Cancelled,
            "the user's answer outranks the producer's"
        );
    }

    #[test]
    fn the_header_is_the_documented_sentence() {
        // The literal, not `query.header()`: this function *is*
        // `query.header()`, so comparing the two only asserts that one line of
        // delegation was not deleted and would survive both of them changing
        // together.
        let mut query = Query::new(VfsPath::local("/srv/dev"));
        query.name = "*.rs".to_string();
        assert_eq!(header(&query), "[search: *.rs in /srv/dev]");

        query.roots.push(VfsPath::local("/srv/other"));
        assert_eq!(header(&query), "[search: *.rs in /srv/dev +1]");
    }
}
