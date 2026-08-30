//! The `ignore` half of a search.
//!
//! > **Walk**: `ignore::WalkBuilder`, parallel across threads, with hidden
//! > files included and gitignore rules **off** by default
//! > (`search.respect_gitignore`) - a file manager should find what is on
//! > disk, not what git would show.
//!
//! **This is the only place in the crate that constructs a `WalkBuilder`**, and
//! [`builder`] is the only place that configures one. Two of `ignore`'s
//! switches read backwards from their intent - `hidden(true)` means *skip*
//! hidden files, and `parents(true)` reads ignore files *above* the root - and
//! getting either inversion wrong in a second place would be invisible from
//! the outside. One place, written out switch by switch, with the reason beside
//! each.
//!
//! # Per entry, cheapest test first
//!
//! 1. **cancelled?** the sink's flag, and nothing else's.
//! 2. **an error?** counted in the [`Tally`] and skipped. the rule
//!    holds: an unreadable directory is reported, never rendered as empty.
//! 3. **the name**, against `Compiled::name_matches`. A directory that fails
//!    the name test is still **descended into**: a mask names what you are
//!    looking for, not where. Only the depth and the restriction prune.
//! 4. **size, date and attributes**, from the walker's own metadata.
//! 5. **the contents**, when the "Find text" box is ticked.
//!
//! # One `stat` per entry and none per hit
//!
//! The row is built from the metadata the walker already has, through
//! [`crate::vfs::local::entry_from_metadata`], so a search of a million files
//! performs a million `stat`s and not two million - and the row it builds is
//! the row a directory listing would have built, rather than a second, subtly
//! different one.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::SystemTime;

use ignore::{DirEntry, WalkState};

use crate::vfs::list::ListSink;
use crate::vfs::{BackendKind, EntryKind, VfsPath};

use super::query::Compiled;

/// The most threads the walk uses, matching ripgrep's own cap.
///
/// Past a dozen, a walk is bounded by the filesystem rather than by the CPU,
/// and the threads only add contention on the sink.
pub const MAX_THREADS: usize = 12;

/// How much of a problem's own words the end-of-walk note quotes.
const MAX_PROBLEM_CHARS: usize = 120;

/// the walker, configured. **The only `WalkBuilder` in the crate.**
///
/// | Call | Value | Why |
/// |---|---|---|
/// | `new` + `add` | every local root | the `>>` button |
/// | `hidden(false)` | always | **an inversion**: `ignore`'s `hidden(true)` means *skip* hidden |
/// | `ignore`, `git_ignore`, `git_global`, `git_exclude` | `!respect_gitignore` | the design |
/// | `parents(!respect)` | | **an inversion of intent**: `parents(true)` reads ignore files above the root |
/// | `require_git(false)` | always | so the setting still works outside a repository |
/// | `max_depth` | the dropdown | the design |
/// | `follow_links` | `ops.follow_symlinks` | a symlink loop is a hang, and `ignore` does not detect one |
/// | `same_file_system(false)` | always | a mount under the root is part of the tree the user is looking at |
/// | `max_filesize(None)` | always | size filtering is the Advanced tab's, which has a lower bound too |
/// | `filter_entry` | when the search is restricted | the "Only search in selected" |
///
/// With **no** local root at all the builder walks the empty path, which yields
/// one error and no entries. [`run`] never reaches that: it answers an empty
/// tally before building anything.
pub fn builder(compiled: &Compiled, threads: usize) -> ignore::WalkBuilder {
    let query = compiled.query();
    let roots = local_roots(compiled);
    let mut builder = match roots.split_first() {
        Some((first, rest)) => {
            let mut builder = ignore::WalkBuilder::new(first);
            for root in rest {
                builder.add(root);
            }
            builder
        }
        None => ignore::WalkBuilder::new(PathBuf::new()),
    };
    let respect = query.respect_gitignore;
    builder
        // "with hidden files included". `ignore` spells this
        // one backwards, which is why it is written out here.
        .hidden(false)
        .ignore(respect)
        .git_ignore(respect)
        .git_global(respect)
        .git_exclude(respect)
        // `parents(true)` reads `.gitignore` files *above* the root, so a
        // search of `~/dev/proj/src` would silently obey `~/dev/.gitignore`.
        .parents(respect)
        // So that `respect_gitignore = true` still works outside a repository.
        .require_git(false)
        .max_depth(query.depth.max_depth())
        .follow_links(query.follow_symlinks)
        // A mount under the root is part of the tree the user is looking at.
        .same_file_system(false)
        // Size filtering is the Advanced tab's, and it has a lower bound too,
        // which this switch cannot express.
        .max_filesize(None)
        .threads(threads.clamp(1, MAX_THREADS));

    // One filter, because `ignore` keeps only the last one it is given. It
    // answers two questions: the restriction, and git's own directory.
    let restrict: Vec<PathBuf> = query
        .restrict
        .iter()
        .filter_map(|p| p.local_path().map(std::path::Path::to_path_buf))
        .collect();
    if !restrict.is_empty() || respect {
        builder.filter_entry(move |entry| {
            if respect && entry.file_name() == GIT_DIR {
                return false;
            }
            restrict.is_empty() || within_restriction(&restrict, entry)
        });
    }
    builder
}

/// The directory `search.respect_gitignore` also excludes.
///
/// `ignore` does not skip `.git` itself - ripgrep does it above the library,
/// with an override - and with hidden files included nothing
/// else would. A user who asked for git's opinion of the tree did not ask for
/// git's *object store*, and forty thousand loose objects is what they would
/// otherwise get. With the setting off, `.git` is walked like any other
/// directory, because then the user asked for what is on disk.
const GIT_DIR: &str = ".git";

/// the "Only search in selected directories/files".
///
/// A path is kept when it is **one of** the restrictions, **inside** one, or an
/// **ancestor** of one. The last clause is what makes the first two reachable:
/// the walk starts at the panel's directory, and pruning it before descending
/// would make a restriction to `src/main.rs` find nothing at all.
fn within_restriction(restrict: &[PathBuf], entry: &DirEntry) -> bool {
    let path = entry.path();
    restrict
        .iter()
        .any(|allowed| path.starts_with(allowed) || allowed.starts_with(path))
}

/// The local roots of a query, in order, without duplicates.
///
/// A root that is not on the local filesystem - inside an archive, and later a
/// remote - is [`super::backend::walk`]'s, because `ignore` "only walks the
/// local filesystem".
fn local_roots(compiled: &Compiled) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for root in &compiled.query().roots {
        if let Some(path) = root.local_path()
            && seen.insert(path.to_path_buf())
        {
            out.push(path.to_path_buf());
        }
    }
    out
}

/// Run the walk, pushing every hit into `sink`.
///
/// Returns when the tree is exhausted or the sink is cancelled.
/// **Blocking**; call it from `spawn_blocking`.
pub fn run(compiled: &Compiled, threads: usize, sink: &ListSink) -> Tally {
    run_collecting_archives(compiled, threads, sink).0
}

/// [`run`], also reporting the archives it walked past.
///
/// the "Search archives" descends into them, and `ignore` cannot:
/// an archive member is not a path the kernel can walk. The candidates are
/// therefore collected here, where the tree is already being walked, and
/// handed to [`super::backend::walk`] afterwards by [`super::run`], which is
/// the one function that holds a [`crate::vfs::Vfs`].
///
/// The list is empty unless the checkbox is on.
pub fn run_collecting_archives(
    compiled: &Compiled,
    threads: usize,
    sink: &ListSink,
) -> (Tally, Vec<PathBuf>) {
    if local_roots(compiled).is_empty() {
        return (Tally::default(), Vec::new());
    }
    let shared = Mutex::new(Shared::default());
    // One instant for the whole search, so "newer than 7 days" means the same
    // thing for the first file and the millionth.
    let now = SystemTime::now();
    let mut visitors = Builder {
        compiled,
        sink,
        shared: &shared,
        now,
    };
    builder(compiled, threads)
        .build_parallel()
        .visit(&mut visitors);
    let shared = shared
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    (shared.tally, shared.archives)
}

/// What every visitor folds its own findings into at the end of its thread.
#[derive(Debug, Default)]
struct Shared {
    tally: Tally,
    archives: Vec<PathBuf>,
}

/// Builds one visitor per walker thread.
struct Builder<'a> {
    compiled: &'a Compiled,
    sink: &'a ListSink,
    shared: &'a Mutex<Shared>,
    now: SystemTime,
}

impl<'s> ignore::ParallelVisitorBuilder<'s> for Builder<'s> {
    fn build(&mut self) -> Box<dyn ignore::ParallelVisitor + 's> {
        Box::new(Visitor {
            compiled: self.compiled,
            sink: self.sink,
            shared: self.shared,
            now: self.now,
            tally: Tally::default(),
            archives: Vec::new(),
        })
    }
}

/// One walker thread's visitor.
///
/// It counts into its **own** tally and flushes once, on drop, rather than
/// locking a shared one per entry: a walk of a million files would otherwise
/// spend itself on the lock.
struct Visitor<'a> {
    compiled: &'a Compiled,
    sink: &'a ListSink,
    shared: &'a Mutex<Shared>,
    now: SystemTime,
    tally: Tally,
    archives: Vec<PathBuf>,
}

impl Drop for Visitor<'_> {
    fn drop(&mut self) {
        let mut shared = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        shared.tally.add(&self.tally);
        shared.archives.append(&mut self.archives);
    }
}

impl ignore::ParallelVisitor for Visitor<'_> {
    fn visit(&mut self, result: Result<DirEntry, ignore::Error>) -> WalkState {
        // One flag, the sink's. `ops::CancelFlag` is deliberately not used
        // here: a second flag with the same meaning is how one of them gets
        // forgotten.
        if self.sink.is_cancelled() {
            return WalkState::Quit;
        }
        let entry = match result {
            Ok(entry) => entry,
            Err(err) => {
                self.tally.problem(err.to_string());
                return WalkState::Skip;
            }
        };
        // The roots themselves are not results: a search of `~/dev` that
        // reported `~/dev` would put the haystack in the panel with the
        // needles. A root that is a *file* is still tested, because naming one
        // is a way of asking about it.
        if entry.depth() == 0 && entry.file_type().is_some_and(|t| t.is_dir()) {
            return WalkState::Continue;
        }
        self.tally.visited = self.tally.visited.saturating_add(1);

        let meta = match entry.metadata() {
            Ok(meta) => meta,
            Err(err) => {
                // Counted and skipped, but still descended into: a directory
                // whose own metadata is unreadable may still list.
                self.tally.problem(err.to_string());
                return WalkState::Continue;
            }
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_dir = meta.is_dir();

        // Under "Search archives", a local file whose name says archive is
        // walked through the `Vfs` afterwards - whether or not it is itself a
        // hit.
        if self.compiled.query().search_archives
            && !is_dir
            && super::backend::looks_like_archive(&name)
        {
            self.archives.push(entry.path().to_path_buf());
        }

        if !self.compiled.name_matches(&name) {
            // Still descended into: a mask names what you are looking for, not
            // where it lives.
            return WalkState::Continue;
        }

        let mut row = crate::vfs::local::entry_from_metadata(name, &meta);
        // `ignore` hands back `lstat` metadata, which cannot say whether a
        // link resolves to a directory, and `Enter` needs to know. One extra
        // `stat`, and only for symlinks.
        if let EntryKind::Symlink { .. } = row.kind {
            row.kind = EntryKind::Symlink {
                to_dir: std::fs::metadata(entry.path()).is_ok_and(|m| m.is_dir()),
            };
        }
        if !self.compiled.attrs_match(&row, self.now) {
            return WalkState::Continue;
        }

        if let Some(content) = self.compiled.content() {
            // A directory has no contents to search, so it cannot contain the
            // text and cannot be shown to be missing it either.
            if is_dir {
                return WalkState::Continue;
            }
            if meta.len() > content.max_bytes() {
                self.tally.skipped_large = self.tally.skipped_large.saturating_add(1);
                return WalkState::Continue;
            }
            match content.search(entry.path()) {
                super::content::Outcome::Match(hit) => row.hit = hit,
                super::content::Outcome::NoMatch => return WalkState::Continue,
                super::content::Outcome::Binary => {
                    self.tally.skipped_binary = self.tally.skipped_binary.saturating_add(1);
                    return WalkState::Continue;
                }
                super::content::Outcome::Skipped(why) => {
                    self.tally.problem(why);
                    return WalkState::Continue;
                }
            }
        }

        row.location = Some(VfsPath::new(BackendKind::Local, entry.path()));
        self.tally.matched = self.tally.matched.saturating_add(1);
        // `false` is the listing saying it is closed or cancelled, which is the
        // second half of one cancellation and not a second flag.
        if self.sink.push(row) {
            WalkState::Continue
        } else {
            WalkState::Quit
        }
    }
}

/// What a walk passed over, for the one line the honesty rule wants
/// at the end.
///
/// Never one message per file: a search of a source tree passes over thousands
/// of binaries, and thousands of messages is silence with extra steps.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tally {
    /// Entries the walk looked at, the roots themselves excluded.
    pub visited: u64,
    /// Rows pushed into the listing.
    pub matched: u64,
    /// Directories and files the walk could not read.
    pub unreadable: u64,
    /// Files skipped because they are binary and the search was not a hex one.
    pub skipped_binary: u64,
    /// Files skipped because they are past the content-search size limit.
    pub skipped_large: u64,
    /// The first reason, for the message. The rest are counted, not kept.
    pub first_problem: Option<String>,
}

impl Tally {
    /// Fold another tally into this one.
    pub fn add(&mut self, other: &Self) {
        self.visited = self.visited.saturating_add(other.visited);
        self.matched = self.matched.saturating_add(other.matched);
        self.unreadable = self.unreadable.saturating_add(other.unreadable);
        self.skipped_binary = self.skipped_binary.saturating_add(other.skipped_binary);
        self.skipped_large = self.skipped_large.saturating_add(other.skipped_large);
        if self.first_problem.is_none() {
            self.first_problem.clone_from(&other.first_problem);
        }
    }

    /// Count one unreadable thing, keeping the first reason.
    fn problem(&mut self, why: String) {
        self.unreadable = self.unreadable.saturating_add(1);
        if self.first_problem.is_none() {
            self.first_problem = Some(why);
        }
    }

    /// `searched 42,110 files; 3 unreadable`, or `None` when nothing was
    /// passed over.
    ///
    /// `ascii` is `ui.ascii_borders`, and decides how a long reason is cropped,
    /// the same way every other cropped string in the program is.
    pub fn note(&self, ascii: bool) -> Option<String> {
        let mut problems = Vec::new();
        if self.unreadable > 0 {
            problems.push(format!(
                "{} unreadable",
                crate::panel::format::thousands(self.unreadable)
            ));
        }
        if self.skipped_binary > 0 {
            problems.push(format!(
                "{} binary",
                crate::panel::format::thousands(self.skipped_binary)
            ));
        }
        if self.skipped_large > 0 {
            problems.push(format!(
                "{} too large",
                crate::panel::format::thousands(self.skipped_large)
            ));
        }
        if problems.is_empty() {
            return None;
        }
        let mut note = format!(
            "searched {} files; {}",
            crate::panel::format::thousands(self.visited),
            problems.join(", ")
        );
        if let Some(first) = self.first_problem.as_ref() {
            note.push_str(" (");
            note.push_str(&crop(first, MAX_PROBLEM_CHARS, ascii));
            note.push(')');
        }
        Some(note)
    }
}

/// `text` cropped to `chars` characters, marked with an ellipsis.
fn crop(text: &str, chars: usize, ascii: bool) -> String {
    let mut out = String::new();
    for (i, ch) in text.chars().enumerate() {
        if i >= chars {
            out.push_str(if ascii { "..." } else { "\u{2026}" });
            break;
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::query::{ContentQuery, Depth, Query, Tri};
    use crate::vfs::list::{ListFs, ListStatus};
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, UNIX_EPOCH};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// A throwaway tree under `$TMPDIR`, removed on drop.
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
                "hcmd-search-{tag}-{pid}-{nanos}-{n}",
                pid = std::process::id()
            ));
            fs::create_dir_all(&root).expect("create temp tree");
            Self { root }
        }

        fn file(&self, rel: &str, body: &str) -> PathBuf {
            let path = self.root.join(rel);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("create parent");
            }
            fs::write(&path, body).expect("write");
            path
        }

        fn query(&self) -> Query {
            Query::new(VfsPath::local(&self.root))
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    /// Run one search to completion, single-threaded so the order is stable.
    fn search(query: &Query) -> (Vec<String>, Tally, ListStatus) {
        let compiled = query.compile().expect("compiles");
        let (listing, sink) = ListFs::streaming("test", &compiled.query().roots);
        let tally = run(&compiled, 1, &sink);
        sink.finish(ListStatus::Complete);
        let (_, rows, status) = listing.snapshot_from(0);
        let mut names: Vec<String> = rows.iter().map(|e| e.name.clone()).collect();
        names.sort();
        (names, tally, status)
    }

    #[test]
    fn a_file_manager_finds_what_is_on_disk() {
        // hidden files included and gitignore rules off by
        // default. A file manager shows what is there, not what git would.
        let t = TempTree::new("ondisk");
        t.file(".gitignore", "ignored.txt\n");
        t.file("ignored.txt", "x");
        t.file(".hidden.txt", "x");
        t.file(".git/config", "x");
        t.file("plain.txt", "x");

        let mut q = t.query();
        let (names, _, _) = search(&q);
        assert!(names.contains(&"ignored.txt".to_string()));
        assert!(names.contains(&".hidden.txt".to_string()));
        assert!(names.contains(&"config".to_string()), "{names:?}");

        // And with the setting on, git's opinion is honoured again: the
        // ignored file goes, and so does git's own directory - which `ignore`
        // does not exclude on its own (see `GIT_DIR`).
        q.respect_gitignore = true;
        let (names, _, _) = search(&q);
        assert!(!names.contains(&"ignored.txt".to_string()), "{names:?}");
        assert!(!names.contains(&"config".to_string()), "{names:?}");
        assert!(!names.contains(&".git".to_string()), "{names:?}");
        assert!(
            names.contains(&".hidden.txt".to_string()),
            "a dotfile is not a gitignore rule: {names:?}"
        );
    }

    #[test]
    fn the_subdirectory_dropdown_counts_levels_not_walker_depth() {
        let t = TempTree::new("depth");
        t.file("top.txt", "x");
        t.file("one/mid.txt", "x");
        t.file("one/two/deep.txt", "x");

        let mut q = t.query();
        q.depth = Depth::None;
        let (names, _, _) = search(&q);
        assert_eq!(names, vec!["one", "top.txt"], "the root's own entries");

        q.depth = Depth::Levels(1);
        let (names, _, _) = search(&q);
        assert!(names.contains(&"mid.txt".to_string()));
        assert!(!names.contains(&"deep.txt".to_string()), "{names:?}");

        q.depth = Depth::Unlimited;
        let (names, _, _) = search(&q);
        assert!(names.contains(&"deep.txt".to_string()));
    }

    #[test]
    fn a_mask_names_what_you_are_looking_for_and_not_where() {
        // A directory that fails the name test is still descended into.
        let t = TempTree::new("mask");
        t.file("src/main.rs", "x");
        t.file("src/notes.txt", "x");

        let mut q = t.query();
        q.name = "*.rs".to_string();
        let (names, tally, _) = search(&q);
        assert_eq!(names, vec!["main.rs"]);
        assert_eq!(tally.matched, 1);
        assert!(tally.visited >= 3, "the walk saw the whole tree");
    }

    #[test]
    fn a_branch_view_lists_the_tree_directories_included() {
        // `Ctrl+B` is "a flat recursive listing of the current
        // tree", so a directory is a row like anything else.
        let t = TempTree::new("branch");
        t.file("src/main.rs", "x");
        t.file("readme.md", "x");

        let (names, _, _) = search(&Query::branch(VfsPath::local(&t.root)));
        assert_eq!(names, vec!["main.rs", "readme.md", "src"]);
    }

    #[test]
    fn every_row_addresses_the_file_and_not_the_listing() {
        // Two rows with one name is the case a flat listing exists for.
        let t = TempTree::new("locations");
        t.file("one/mod.rs", "x");
        t.file("two/mod.rs", "x");

        let mut q = t.query();
        q.name = "mod.rs".to_string();
        let compiled = q.compile().expect("compiles");
        let (listing, sink) = ListFs::streaming("test", &compiled.query().roots);
        run(&compiled, 1, &sink);
        sink.finish(ListStatus::Complete);
        let (_, rows, _) = listing.snapshot_from(0);
        assert_eq!(rows.len(), 2);
        let mut homes: Vec<String> = rows
            .iter()
            .filter_map(|r| r.location.as_ref().map(ToString::to_string))
            .collect();
        homes.sort();
        assert!(homes.first().is_some_and(|h| h.ends_with("one/mod.rs")));
        assert!(homes.get(1).is_some_and(|h| h.ends_with("two/mod.rs")));
    }

    #[test]
    fn esc_stops_the_walk_and_keeps_what_was_found() {
        // "`Esc` stops the walk and keeps what was found."
        let t = TempTree::new("cancel");
        for i in 0..200 {
            t.file(&format!("d{}/f{i:04}.txt", i % 8), "x");
        }
        let compiled = t.query().compile().expect("compiles");
        let (listing, sink) = ListFs::streaming("test", &compiled.query().roots);

        // Cancelled before it starts: the walk stops at its first entry and
        // the listing keeps whatever had arrived, which here is nothing.
        sink.cancel();
        let tally = run(&compiled, 1, &sink);
        sink.finish(ListStatus::Complete);

        assert!(tally.matched < 200, "the walk did not run to the end");
        assert_eq!(listing.len(), usize::try_from(tally.matched).unwrap_or(0));
        assert_eq!(
            listing.status(),
            ListStatus::Cancelled,
            "the user's answer outranks the producer's"
        );
    }

    #[test]
    fn a_cancelled_walk_keeps_the_rows_it_had_already_pushed() {
        let t = TempTree::new("keep");
        for i in 0..64 {
            t.file(&format!("f{i:04}.txt"), "x");
        }
        let compiled = t.query().compile().expect("compiles");
        let (listing, sink) = ListFs::streaming("test", &compiled.query().roots);
        // Half a walk: push a row of our own first, then cancel from the
        // "user's" side while the walk has not started.
        assert!(sink.push(crate::vfs::Entry::file("already-here")));
        sink.cancel();
        run(&compiled, 1, &sink);
        let (_, rows, status) = listing.snapshot_from(0);
        assert_eq!(rows.len(), 1, "what was found stays: {rows:?}");
        assert_eq!(status, ListStatus::Cancelled);
    }

    #[test]
    fn a_content_search_finds_the_line_and_carries_it_back() {
        let t = TempTree::new("content");
        t.file("a.txt", "nothing here\n");
        t.file("b.txt", "one\nTODO: this one\n");

        let mut q = t.query();
        q.content = Some(ContentQuery {
            pattern: "TODO".to_string(),
            ..ContentQuery::default()
        });
        let compiled = q.compile().expect("compiles");
        let (listing, sink) = ListFs::streaming("test", &compiled.query().roots);
        run(&compiled, 1, &sink);
        sink.finish(ListStatus::Complete);
        let (_, rows, _) = listing.snapshot_from(0);
        assert_eq!(rows.len(), 1);
        let row = rows.first().expect("one row");
        assert_eq!(row.name, "b.txt");
        let hit = row.hit.as_ref().expect("a content hit");
        assert_eq!(hit.line, Some(2));
        assert_eq!(hit.line_text, "TODO: this one");
    }

    #[test]
    fn an_inverted_content_search_reports_the_other_files() {
        let t = TempTree::new("inverted");
        t.file("a.txt", "nothing here\n");
        t.file("b.txt", "TODO: this one\n");

        let mut q = t.query();
        q.content = Some(ContentQuery {
            pattern: "TODO".to_string(),
            inverted: true,
            ..ContentQuery::default()
        });
        let (names, _, _) = search(&q);
        assert_eq!(names, vec!["a.txt"]);

        // A directory is in neither answer: it has no contents to search.
        let t = TempTree::new("inverted-dirs");
        t.file("sub/a.txt", "TODO\n");
        let mut q = t.query();
        q.content = Some(ContentQuery {
            pattern: "TODO".to_string(),
            inverted: true,
            ..ContentQuery::default()
        });
        let (names, _, _) = search(&q);
        assert!(names.is_empty(), "{names:?}");
    }

    #[test]
    fn a_binary_file_is_counted_rather_than_reported_per_file() {
        let t = TempTree::new("binary");
        fs::write(t.root.join("a.bin"), b"\x00\x01TODO").expect("write");
        t.file("b.txt", "TODO\n");

        let mut q = t.query();
        q.content = Some(ContentQuery {
            pattern: "TODO".to_string(),
            ..ContentQuery::default()
        });
        let (names, tally, _) = search(&q);
        assert_eq!(names, vec!["b.txt"]);
        assert_eq!(tally.skipped_binary, 1);
        let note = tally.note(true).expect("a note");
        assert!(note.contains("1 binary"), "{note}");
    }

    #[test]
    fn the_advanced_filters_reach_the_walk() {
        let t = TempTree::new("advanced");
        t.file("small.txt", "x");
        t.file("large.txt", &"x".repeat(4096));
        t.file(".hidden.txt", "x");

        let mut q = t.query();
        q.size.min = Some(1024);
        let (names, _, _) = search(&q);
        assert_eq!(names, vec!["large.txt"]);

        let mut q = t.query();
        q.attrs.hidden = Tri::Yes;
        let (names, _, _) = search(&q);
        assert_eq!(names, vec![".hidden.txt"]);
    }

    #[test]
    fn only_search_in_selected_prunes_the_walk() {
        // the "Only search in selected directories/files", which
        // must still be able to reach a file inside a directory it restricts
        // to.
        let t = TempTree::new("restrict");
        t.file("keep/a.txt", "x");
        t.file("keep/deep/b.txt", "x");
        t.file("drop/c.txt", "x");
        t.file("named.txt", "x");

        let mut q = t.query();
        q.restrict = vec![
            VfsPath::local(t.root.join("keep")),
            VfsPath::local(t.root.join("named.txt")),
        ];
        let (names, _, _) = search(&q);
        assert_eq!(names, vec!["a.txt", "b.txt", "deep", "keep", "named.txt"]);
    }

    #[test]
    fn an_unreadable_directory_is_counted_and_never_rendered_as_empty() {
        // the rule, applied to a walk: the tally carries the count
        // and the first reason, and the walk keeps going.
        let mut tally = Tally::default();
        tally.problem("one/: permission denied".to_string());
        tally.problem("two/: permission denied".to_string());
        assert_eq!(tally.unreadable, 2);
        assert_eq!(
            tally.first_problem.as_deref(),
            Some("one/: permission denied"),
            "the first reason is kept and the rest are counted"
        );

        let mut other = Tally {
            visited: 10,
            matched: 2,
            ..Tally::default()
        };
        other.add(&tally);
        assert_eq!(other.unreadable, 2);
        assert_eq!(other.visited, 10);
        let note = other.note(true).expect("a note");
        assert!(note.contains("2 unreadable"), "{note}");
        assert!(note.contains("permission denied"), "{note}");
    }

    #[test]
    fn a_walk_that_passed_over_nothing_says_nothing() {
        let tally = Tally {
            visited: 42_110,
            matched: 7,
            ..Tally::default()
        };
        assert_eq!(
            tally.note(true),
            None,
            "silence when there is nothing to say"
        );
    }

    #[test]
    fn a_long_reason_is_cropped_the_way_every_other_string_is() {
        let mut tally = Tally::default();
        tally.problem("x".repeat(400));
        let ascii = tally.note(true).expect("a note");
        assert!(ascii.contains("..."), "{ascii}");
        let unicode = tally.note(false).expect("a note");
        assert!(unicode.contains('\u{2026}'), "{unicode}");
    }

    /// Every source file of the search engine, for the two guard tests below.
    const SOURCES: &[(&str, &str)] = &[
        ("mod.rs", include_str!("mod.rs")),
        ("walk.rs", include_str!("walk.rs")),
        ("backend.rs", include_str!("backend.rs")),
        ("content.rs", include_str!("content.rs")),
        ("query.rs", include_str!("query.rs")),
    ];

    #[test]
    fn the_walker_is_configured_where_it_is_documented() {
        // Not a behaviour test: a guard that the one place a `WalkBuilder` is
        // built is this module, because two of `ignore`'s switches read
        // backwards and a second copy of them would be invisible from the
        // outside.
        let needle = concat!("WalkBuilder", "::new");
        for (name, source) in SOURCES {
            if *name == "walk.rs" {
                assert!(source.contains(needle), "this is the one place");
            } else {
                assert!(
                    !source.contains(needle),
                    "{name} builds a second walker; the configuration lives in walk.rs"
                );
            }
        }
    }

    #[test]
    fn the_search_engine_never_spawns_a_process() {
        // nothing shells out. The walk is `ignore`, the search is
        // `grep-searcher`, the matching is `grep-regex`. The needles are built
        // from pieces so that this test does not trip over its own source.
        let needles = [
            concat!("std::process", "::Command"),
            concat!("Command", "::new"),
            concat!("std::process", "::exit"),
        ];
        for (name, source) in SOURCES {
            for needle in needles {
                assert!(!source.contains(needle), "{name} spawns something");
            }
        }
    }
}
