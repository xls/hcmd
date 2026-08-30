//! Comparing the two lists.
//!
//! A **marking** operation. It opens no window, produces no report and does
//! not touch the cursor, so everything that already works on marks works on
//! its result - "`F5` copies the differences over, `F8` deletes them, the
//! status line counts them".
//!
//! # The rule, and where each half of it lives
//!
//! the design decides "same" in four steps, in order, stopping at the first
//! answer. [`verdict`] is those four steps over one facing pair and is pure:
//! it takes two [`Entry`] values and a slack, touches no filesystem, and can
//! therefore be tested exhaustively without a directory anywhere.
//! [`compare_lists`] runs it over two whole listings and answers in the only
//! currency a panel understands, which is a set of names to mark.
//!
//! The fifth step - the one the design puts behind `ops.compare_contents` -
//! is not a step at all but a job, because it reads bytes: [`run`] is the
//! [`crate::ops::JobKind::Compare`] worker and [`bytes_differ`] is the
//! streaming comparison underneath it.
//!
//! # What "the two lists" means
//!
//! The two panels' active tabs **as they stand**: hidden files excluded when
//! `panel.show_hidden` is off, mask-filtered rows excluded when a filter is
//! on. A panel showing a virtual listing (search results, `Ctrl+B` branch
//! view) compares by name like any other list rather than being
//! refused; that is the honest reading of "the two lists", and it is said here
//! rather than discovered by a reader of the code.
//!

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use crate::error::Error;
use crate::vfs::{Entry, Vfs, VfsPath};

/// Why one entry is not the same as the one facing it.
///
/// The four steps in order, as an enum, so a test asserts on the step that
/// decided rather than on a boolean.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Step 1: the name is on this side only.
    ///
    /// Produced by [`compare_lists`], which is the only place that can see one
    /// list without the other; [`verdict`] has both entries in hand and never
    /// returns it.
    OnlySide,
    /// Step 1: the name is on both sides but one is a directory and the other
    /// is not.
    KindDiffers,
    /// Step 2: the sizes differ.
    SizeDiffers,
    /// Step 3: the mtimes differ by more than `ops.compare_mtime_slack`.
    MtimeDiffers,
    /// Step 4 with `ops.compare_contents` on: the bytes differ.
    ContentsDiffer,
    /// Step 4: the same.
    Same,
    /// Step 4 with `ops.compare_contents` on: same so far, and the job has
    /// not read them yet.
    Undecided,
}

impl Verdict {
    /// Whether this verdict marks the entry.
    ///
    /// [`Verdict::Undecided`] does **not**: the design marks what "is not the
    /// same on the other side, and nothing else", and a pair the first three
    /// steps could not separate is not yet known to differ. The job says so
    /// later, through [`crate::app::App::finish_compare`].
    pub const fn marks(self) -> bool {
        match self {
            Self::OnlySide
            | Self::KindDiffers
            | Self::SizeDiffers
            | Self::MtimeDiffers
            | Self::ContentsDiffer => true,
            Self::Same | Self::Undecided => false,
        }
    }
}

/// What [`compare_lists`] decided.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Outcome {
    /// Names to mark on the left. Replaces the panel's marks entirely -
    /// the design marks "every entry that is not the same on the other side,
    /// **and nothing else**".
    pub left: HashSet<String>,
    /// Names to mark on the right.
    pub right: HashSet<String>,
    /// Pairs that steps 1 to 3 called the same and that
    /// `ops.compare_contents` sends to the job. Empty when the option is off.
    pub undecided: Vec<String>,
}

impl Outcome {
    /// How many **entries** differ, counting a name that differs on both sides
    /// once.
    ///
    /// Not `left.len() + right.len()`: a file that is on both sides with two
    /// different sizes is one difference and two marks, and "2 differ" for one
    /// changed file would be a lie the status line tells every time.
    pub fn differing(&self) -> usize {
        self.left.union(&self.right).count()
    }

    /// True when neither side is marked and nothing is waiting on the job.
    pub fn is_empty(&self) -> bool {
        self.left.is_empty() && self.right.is_empty() && self.undecided.is_empty()
    }
}

/// the four steps over one facing pair.
///
/// Pure and total: no filesystem access, so the whole rule is tested from two
/// [`Entry`] values. `slack` is `ops.compare_mtime_slack`.
///
/// Two cases the design does not name are decided here rather than four
/// times over:
///
/// * **One side is a directory and the other is not.** [`Verdict::KindDiffers`]
///   at step 1, before sizes are looked at: a directory's size is "zero and
///   meaningless" ([`Entry::size`]'s own doc comment), so step 2 would compare
///   a real size against a zero and answer "different" for the right reason by
///   accident.
/// * **Either side reports no mtime at all**, which a remote backend may
///   legitimately do. Step 3 cannot answer and is **skipped**; the pair falls
///   through to step 4 and is not marked. Marking what cannot be shown to
///   differ is the opposite of what a comparison is for, and a remote panel
///   would otherwise report every file as changed.
///
/// Two directories are [`Verdict::Same`] whatever their sizes and mtimes say:
/// the "directories compare by name only", because "marking it
/// would mean marking a whole subtree the user did not ask about".
pub fn verdict(a: &Entry, b: &Entry, slack: Duration, contents: bool) -> Verdict {
    // Step 1, second half: the name is on both sides but they are not the same
    // kind of thing.
    if a.is_dir() != b.is_dir() {
        return Verdict::KindDiffers;
    }
    // directories compare by name only. Nothing below this line
    // runs for one, which is what keeps `Ctrl+B` the way to compare trees.
    if a.is_dir() {
        return Verdict::Same;
    }
    // Step 2.
    if a.size != b.size {
        return Verdict::SizeDiffers;
    }
    // Step 3, skipped when either side cannot answer.
    if let (Some(ma), Some(mb)) = (a.mtime, b.mtime) {
        let apart = if ma >= mb {
            ma.duration_since(mb).ok()
        } else {
            mb.duration_since(ma).ok()
        };
        if apart.is_some_and(|d| d > slack) {
            return Verdict::MtimeDiffers;
        }
    }
    // Step 4. `ops.compare_contents` turns the answer into a question for the
    // job rather than into a comparison performed here: the design says
    // "contents are not read" behind a key that looks instant.
    if contents {
        Verdict::Undecided
    } else {
        Verdict::Same
    }
}

/// the design over two whole lists.
///
/// `..` is never compared and never marked ([`Entry::is_parent`] "never sorts,
/// never marks and never counts"). Directories present on both sides are the
/// same whatever is inside them, which is the "directories compare
/// by name only".
///
/// The pairing is by name, so two panels showing the same directory compare it
/// against itself and mark nothing, which is correct and needs no special
/// case. A flat virtual listing can hold two rows with the same name from
/// different directories; they fold onto one name here, and
/// [`crate::app::App::compare_lists`] marks every row carrying a marked name,
/// which is the only answer that is stable under a re-sort.
pub fn compare_lists<'a>(
    left: &'a [Entry],
    right: &'a [Entry],
    slack: Duration,
    contents: bool,
) -> Outcome {
    // Borrowed, not cloned: two directories of a million rows are compared
    // without copying either of them.
    let index = |list: &'a [Entry]| -> HashMap<&'a str, &'a Entry> {
        list.iter()
            .filter(|e| !e.is_parent)
            .map(|e| (e.name.as_str(), e))
            .collect()
    };
    let by_name_right = index(right);
    let mut out = Outcome::default();

    for entry in left.iter().filter(|e| !e.is_parent) {
        let Some(other) = by_name_right.get(entry.name.as_str()) else {
            // Step 1: on this side only.
            out.left.insert(entry.name.clone());
            continue;
        };
        let what = verdict(entry, other, slack, contents);
        if what.marks() {
            out.left.insert(entry.name.clone());
            out.right.insert(entry.name.clone());
        } else if matches!(what, Verdict::Undecided) {
            out.undecided.push(entry.name.clone());
        }
    }

    let by_name_left = index(left);
    for entry in right.iter().filter(|e| !e.is_parent) {
        if !by_name_left.contains_key(entry.name.as_str()) {
            out.right.insert(entry.name.clone());
        }
    }

    // A stable order, so the job's progress runs down the listing rather than
    // wherever the hash landed, and so a test can assert on it.
    out.undecided.sort_unstable();
    out
}

/// The message the status line gets: `12 differ of 340 compared`, or
/// `the two lists are identical`.
///
/// `compared` is how many entries were looked at, which the caller knows and
/// this does not: an [`Outcome`] carries only what differs.
pub fn describe(outcome: &Outcome, compared: usize) -> String {
    if outcome.is_empty() {
        return "the two lists are identical".to_string();
    }
    format!("{} differ of {compared} compared", outcome.differing())
}

/// The floor under the window [`bytes_differ`] reads.
///
/// The window itself comes from [`crate::ops::chunk_size`], which answers from
/// [`crate::vfs::Capabilities`] - one loop reads both
/// sides, so the source side's backend decides, arbitrarily and consistently.
/// This is only insurance against a backend that answers with something too
/// small to make progress worth reporting.
const MIN_WINDOW: usize = 4096;

/// Whether two files differ, byte for byte, streaming.
///
/// Reads both through the [`Vfs`] in fixed windows and stops at the first
/// difference. Never loads a file whole, for the same reason the viewer never
/// does. `tick` is the cancellation and progress hook, called per window with
/// the bytes read so far **on one side**; returning false abandons the
/// comparison with [`Error::Cancelled`], which the runner reports as a cancel
/// rather than as a per-file failure.
///
/// A short read is not an answer: both sides are filled to the window before
/// they are compared, so a backend that hands back 300 bytes at a time cannot
/// make two identical files look different.
pub fn bytes_differ(
    vfs: &dyn Vfs,
    a: &VfsPath,
    b: &VfsPath,
    tick: &mut dyn FnMut(u64) -> bool,
) -> crate::Result<bool> {
    Ok(first_difference(vfs, a, b, tick)?.is_some())
}

/// The offset of the first byte at which two files differ, or `None` when they
/// are the same file byte for byte.
///
/// The same comparison [`bytes_differ`] answers a boolean from, which delegates
/// here: the marking pass wants only "same or not", and comparing two files
/// directly wants to say where. One implementation, so the two answers cannot
/// disagree about the same pair of files.
///
/// Where one file is a prefix of the other the offset is the length of the
/// shorter, which is the first position at which they stop agreeing.
pub fn first_difference(
    vfs: &dyn Vfs,
    a: &VfsPath,
    b: &VfsPath,
    tick: &mut dyn FnMut(u64) -> bool,
) -> crate::Result<Option<u64>> {
    let window = crate::ops::chunk_size(&vfs.capabilities_for(a)).max(MIN_WINDOW);
    let mut left = vfs.open_read(a)?;
    let mut right = vfs.open_read(b)?;
    let mut buf_left = vec![0_u8; window];
    let mut buf_right = vec![0_u8; window];
    let mut read = 0_u64;

    loop {
        let got_left = fill(left.as_mut(), &mut buf_left)?;
        let got_right = fill(right.as_mut(), &mut buf_right)?;
        // Different lengths are a difference, and this is where a file that
        // grew between the listing and the job is caught: step 2 compared the
        // sizes the listing reported, and these are the bytes that are there.
        if got_left != got_right {
            // One ended first. They agreed up to that point, so the shorter
            // file's end is where they stop agreeing.
            let shorter = got_left.min(got_right);
            let common = common_prefix(
                buf_left.get(..shorter).unwrap_or_default(),
                buf_right.get(..shorter).unwrap_or_default(),
            );
            return Ok(Some(
                read.saturating_add(u64::try_from(common).unwrap_or(u64::MAX)),
            ));
        }
        if got_left == 0 {
            return Ok(None);
        }
        // `get` rather than an index: the slice length is the read count and
        // the buffer is `window` long, so this cannot be `None`, and the code
        // that proves it is one line away from the code that relies on it.
        if buf_left.get(..got_left) != buf_right.get(..got_right) {
            let common = common_prefix(
                buf_left.get(..got_left).unwrap_or_default(),
                buf_right.get(..got_right).unwrap_or_default(),
            );
            return Ok(Some(
                read.saturating_add(u64::try_from(common).unwrap_or(u64::MAX)),
            ));
        }
        read = read.saturating_add(u64::try_from(got_left).unwrap_or(u64::MAX));
        if !tick(read) {
            return Err(Error::Cancelled);
        }
    }
}

/// How many leading bytes two windows share.
///
/// Only reached once the two windows are known to differ, so the answer is
/// always inside at least one of them.
fn common_prefix(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

/// Read until `buf` is full or the source ends, returning how much arrived.
///
/// `std::io::Read::read` is allowed to return less than was asked for at any
/// time and for any reason; a comparison that treated a short read as the end
/// of the file would report two identical files as differing whenever the two
/// backends chunked differently.
fn fill(source: &mut dyn std::io::Read, buf: &mut [u8]) -> crate::Result<usize> {
    let mut done = 0_usize;
    loop {
        let Some(rest) = buf.get_mut(done..) else {
            return Ok(done);
        };
        if rest.is_empty() {
            return Ok(done);
        }
        match source.read(rest) {
            Ok(0) => return Ok(done),
            Ok(n) => done = done.saturating_add(n),
            // An interrupted read is not a failed one.
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
            Err(err) => return Err(Error::Bare(err)),
        }
    }
}

/// The [`crate::ops::JobKind::Compare`] worker.
///
/// `spec.sources` and `spec.targets` are the facing pairs, positionally
/// matched exactly as [`crate::ops::JobKind::Rename`]'s are. It reports the
/// two bars of the design - files done of files total, bytes read of bytes
/// total - and hands the differing **names** back in
/// [`crate::ops::JobSummary::differing`].
///
/// The byte total counts **one side** of each pair, which is not an
/// approximation: the pairs reached the job only because step 2 found their
/// sizes equal.
///
/// Writes nothing, anywhere. A pair that cannot be read is one
/// [`crate::ops::JobContext::fail`] and the batch continues;
/// it is not reported as a difference, because "could not be read" and "is not
/// the same" are different answers and only one of them is this job's.
pub fn run(vfs: &dyn Vfs, spec: &crate::ops::JobSpec, ctx: &mut crate::ops::JobContext) {
    debug_assert!(matches!(
        spec.kind,
        crate::ops::JobKind::Compare | crate::ops::JobKind::CompareFiles
    ));
    let count = spec.sources.len().min(spec.targets.len());
    // One `stat` per pair, kept: on a network backend it is a round trip, and
    // the same figure drives both the batch total and each file's own bar.
    let sizes: Vec<u64> = spec
        .sources
        .iter()
        .take(count)
        .map(|source| vfs.stat(source).map_or(0, |entry| entry.size))
        .collect();
    let bytes_total = sizes.iter().fold(0_u64, |acc, n| acc.saturating_add(*n));
    ctx.start(u64::try_from(count).unwrap_or(u64::MAX), bytes_total);

    for index in 0..count {
        if ctx.cancelled() {
            break;
        }
        let (Some(a), Some(b)) = (spec.sources.get(index), spec.targets.get(index)) else {
            continue;
        };
        ctx.set_file(
            &a.to_string(),
            sizes.get(index).copied().unwrap_or_default(),
        );

        // The hook forwards the delta since the last window, because
        // `JobContext` counts bytes cumulatively across the whole batch while
        // `bytes_differ` counts them within one pair.
        let mut last = 0_u64;
        let mut tick = |so_far: u64| -> bool {
            let delta = so_far.saturating_sub(last);
            last = so_far;
            ctx.add_bytes(delta)
        };
        match first_difference(vfs, a, b, &mut tick) {
            Ok(Some(at)) => {
                let name = a.file_name().unwrap_or_else(|| a.to_string());
                ctx.add_differing(name);
                // The offset is only an answer about one pair, so it is
                // recorded only for the job whose question was about one.
                if spec.kind == crate::ops::JobKind::CompareFiles {
                    ctx.set_first_difference(at);
                }
                ctx.add_file();
            }
            Ok(None) => ctx.add_file(),
            // A cancel is not a failure: the flag is already set and the
            // summary says `cancelled`, so nothing is added to the list of
            // reasons a user has to read.
            Err(Error::Cancelled) => break,
            Err(err) => ctx.fail(a, err),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    const SLACK: Duration = Duration::from_secs(2);

    fn file(name: &str, size: u64, mtime_secs: u64) -> Entry {
        Entry {
            size,
            mtime: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(mtime_secs)),
            ..Entry::file(name)
        }
    }

    #[test]
    fn the_four_steps_run_in_order_and_stop_at_the_first_answer() {
        // invariant I7. Each pair below differs at exactly one
        // step and agrees at every later one, so the step named is the step
        // that decided.
        let a = file("a", 100, 1_000);

        // Step 2: same mtime, different size.
        let bigger = file("a", 200, 1_000);
        assert_eq!(verdict(&a, &bigger, SLACK, false), Verdict::SizeDiffers);

        // Step 3: same size, mtimes more than the slack apart.
        let later = file("a", 100, 1_003);
        assert_eq!(verdict(&a, &later, SLACK, false), Verdict::MtimeDiffers);

        // Step 4: same size, mtimes within the slack.
        let close = file("a", 100, 1_002);
        assert_eq!(
            verdict(&a, &close, SLACK, false),
            Verdict::Same,
            "two seconds is the FAT resolution the design names, and is not a difference"
        );
        assert_eq!(verdict(&a, &a, SLACK, false), Verdict::Same);
    }

    #[test]
    fn a_directory_is_compared_by_name_alone() {
        // "Directories compare by name only." Whatever the size
        // and mtime columns say, two directories with the same name are the
        // same - marking one would mean marking a whole subtree.
        let mut one = Entry::dir("src");
        one.size = 4096;
        one.mtime = Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1));
        let mut two = Entry::dir("src");
        two.size = 8192;
        two.mtime = Some(SystemTime::UNIX_EPOCH + Duration::from_secs(9_000));
        assert_eq!(verdict(&one, &two, SLACK, false), Verdict::Same);
        // And with the contents option on it is still `Same`, never
        // `Undecided`: a directory is never sent to the job.
        assert_eq!(verdict(&one, &two, SLACK, true), Verdict::Same);
    }

    #[test]
    fn a_directory_facing_a_file_of_the_same_name_differs_at_step_one() {
        // decided before sizes are looked at,
        // because a directory's size is zero and meaningless.
        let dir = Entry::dir("build");
        let plain = file("build", 0, 1_000);
        assert_eq!(verdict(&dir, &plain, SLACK, false), Verdict::KindDiffers);
        assert!(Verdict::KindDiffers.marks());
    }

    #[test]
    fn a_missing_mtime_skips_step_three_instead_of_marking_everything() {
        // a backend that reports no mtime cannot
        // answer step 3, and a remote panel would otherwise report every file
        // as changed.
        let known = file("a", 100, 1_000);
        let unknown = Entry {
            size: 100,
            mtime: None,
            ..Entry::file("a")
        };
        assert_eq!(verdict(&known, &unknown, SLACK, false), Verdict::Same);
        assert_eq!(verdict(&unknown, &known, SLACK, false), Verdict::Same);
        // The size still decides, because step 2 is above step 3.
        let unknown_bigger = Entry {
            size: 101,
            mtime: None,
            ..Entry::file("a")
        };
        assert_eq!(
            verdict(&known, &unknown_bigger, SLACK, false),
            Verdict::SizeDiffers
        );
    }

    #[test]
    fn only_the_differences_are_marked_and_nothing_else() {
        // the design marks "every entry that is not the same on the other
        // side, and nothing else".
        let left = vec![
            Entry::parent_entry(),
            file("same", 10, 100),
            file("bigger", 10, 100),
            file("left-only", 10, 100),
            Entry::dir("shared"),
        ];
        let right = vec![
            Entry::parent_entry(),
            file("same", 10, 100),
            file("bigger", 20, 100),
            file("right-only", 10, 100),
            Entry::dir("shared"),
        ];
        let out = compare_lists(&left, &right, SLACK, false);
        assert_eq!(
            out.left,
            ["bigger", "left-only"]
                .iter()
                .map(|s| (*s).to_string())
                .collect()
        );
        assert_eq!(
            out.right,
            ["bigger", "right-only"]
                .iter()
                .map(|s| (*s).to_string())
                .collect()
        );
        assert!(out.undecided.is_empty(), "contents were not asked for");
        assert!(
            !out.left.contains("..") && !out.right.contains(".."),
            "the parent row is never compared and never marked"
        );
        assert_eq!(out.differing(), 3, "bigger, left-only, right-only");
    }

    #[test]
    fn two_identical_listings_mark_nothing() {
        // Two panels on the same directory compare it against itself, which
        // needs no special case.
        let list = vec![Entry::parent_entry(), file("a", 1, 1), Entry::dir("b")];
        let out = compare_lists(&list, &list, SLACK, false);
        assert!(out.is_empty());
        assert_eq!(describe(&out, 2), "the two lists are identical");
    }

    #[test]
    fn the_contents_option_defers_the_pairs_the_first_three_steps_could_not_separate() {
        // steps 1 to 3 mark exactly as before, without reading a
        // byte; what changes is step 4.
        let left = vec![file("same", 10, 100), file("bigger", 10, 100)];
        let right = vec![file("same", 10, 100), file("bigger", 20, 100)];
        let out = compare_lists(&left, &right, SLACK, true);
        assert_eq!(out.undecided, vec!["same".to_string()]);
        assert!(out.left.contains("bigger"), "step 2 still marks in place");
        assert!(
            !out.left.contains("same"),
            "an undecided pair is not marked"
        );
        assert!(!Verdict::Undecided.marks());
    }

    #[test]
    fn the_status_line_counts_entries_rather_than_marks() {
        let mut out = Outcome::default();
        out.left.insert("a".to_string());
        out.right.insert("a".to_string());
        assert_eq!(
            describe(&out, 340),
            "1 differ of 340 compared",
            "one changed file is one difference and two marks"
        );
    }

    #[test]
    fn a_short_read_is_not_the_end_of_the_file() {
        // `fill` is what keeps two identical files from looking different on a
        // backend that hands back a few bytes at a time.
        struct Dribble(Vec<u8>, usize);
        impl std::io::Read for Dribble {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let Some(byte) = self.0.get(self.1).copied() else {
                    return Ok(0);
                };
                let Some(slot) = buf.first_mut() else {
                    return Ok(0);
                };
                *slot = byte;
                self.1 = self.1.saturating_add(1);
                Ok(1)
            }
        }
        let mut source = Dribble(b"hello world".to_vec(), 0);
        let mut buf = [0_u8; 32];
        let got = fill(&mut source, &mut buf).expect("a dribbling reader still fills");
        assert_eq!(got, 11);
        assert_eq!(buf.get(..got), Some(&b"hello world"[..]));
    }

    /// Keeps concurrently running comparisons out of each other's directory.
    ///
    /// A counter rather than anything derived from the bytes: two empty slices
    /// share one dangling pointer, so keying on the address put two tests that
    /// each compare an empty file in the same directory, where they raced and
    /// read each other's operands.
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    /// Two files on disk, and where the comparison says they part company.
    fn diff_at(a: &[u8], b: &[u8]) -> Option<u64> {
        let tag = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("hcmd-cmp-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a directory to compare in");
        let (left, right) = (dir.join("left"), dir.join("right"));
        std::fs::write(&left, a).expect("the left file");
        std::fs::write(&right, b).expect("the right file");
        let fs_impl = crate::vfs::LocalFs::new();
        let answer = first_difference(
            &fs_impl,
            &VfsPath::local(&left),
            &VfsPath::local(&right),
            &mut |_| true,
        )
        .expect("a readable pair");
        let _ = std::fs::remove_dir_all(&dir);
        answer
    }

    #[test]
    fn two_identical_files_have_no_first_difference() {
        assert_eq!(diff_at(b"the same bytes", b"the same bytes"), None);
        assert_eq!(diff_at(b"", b""), None, "two empty files are the same file");
    }

    #[test]
    fn the_offset_is_where_the_two_files_stop_agreeing() {
        assert_eq!(diff_at(b"abcdef", b"abcXef"), Some(3));
        assert_eq!(
            diff_at(b"Xbcdef", b"abcdef"),
            Some(0),
            "the very first byte"
        );
    }

    #[test]
    fn a_file_that_is_a_prefix_of_the_other_differs_at_the_shorter_ones_end() {
        // Not "they are the same as far as they go": one has bytes the other
        // does not, and the first of those is where they part.
        assert_eq!(diff_at(b"abc", b"abcdef"), Some(3));
        assert_eq!(diff_at(b"abcdef", b"abc"), Some(3));
        assert_eq!(diff_at(b"", b"a"), Some(0));
    }

    #[test]
    fn a_difference_past_the_first_window_is_still_found_at_its_own_offset() {
        // Larger than any window the chunk size picks, so the answer depends
        // on the running offset being carried across reads rather than reset.
        let mut a = vec![b'z'; 5_000_000];
        let mut b = a.clone();
        if let (Some(x), Some(y)) = (a.get_mut(4_321_000), b.get_mut(4_321_000)) {
            *x = 1;
            *y = 2;
        }
        assert_eq!(diff_at(&a, &b), Some(4_321_000));
    }
}
