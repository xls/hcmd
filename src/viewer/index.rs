//! The line/offset index, built incrementally in the background.
//!
//!
//! > The viewer holds a `Read + Seek` handle plus a **line/offset index** built
//! > incrementally in a background task, in `viewer.index_chunk` steps (1 MiB
//! > default). … While the index is still building, `End` and percentage seeks
//! > are marked approximate in the status line rather than blocked. The file
//! > opens instantly; it does not wait for the index.
//!
//! # Why the index is sparse, and why it has to be
//!
//! The obvious index is one offset per line. On a 40 GB log that is a billion
//! lines and eight gigabytes of `u64`, which fails the memory rule
//! ("a function of the window size, not the file size") just as badly as
//! reading the file would. So the index keeps a **checkpoint every
//! `index_chunk` bytes** - an `(offset, line number)` pair at a line start -
//! and finds any individual line by scanning forward from the checkpoint below
//! it. Both halves are bounded: the checkpoint list by [`MAX_CHECKPOINTS`], the
//! forward scan by the checkpoint spacing.
//!
//! When a file is large enough that even one checkpoint per chunk would exceed
//! the cap, the index **decimates**: every second checkpoint is dropped and the
//! spacing doubles. Memory stays under
//! `MAX_CHECKPOINTS * size_of::<Checkpoint>()` - 1 MiB - for any file size at
//! all, and the price is a longer forward scan, which is bounded reading rather
//! than unbounded remembering.
//!
//! # What the index is *not* used for
//!
//! Scrolling. The viewer's position is a byte offset (see
//! [`crate::viewer::Viewer`]), so a line down is "find the next `\n`" and a line
//! up is "find the previous one" - both local, both bounded, and both correct
//! before the index has reached that far. The index answers the three questions
//! that genuinely need the whole file: what line number is this, how many lines
//! are there, and where is line *n*.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::mpsc;

use super::source::{Source, WindowLen};

/// How many checkpoints are kept before the index decimates.
///
/// 65,536 × 24 bytes ≈ 1.5 MiB, and it is the *ceiling for every file*: a
/// 4 TB file holds the same number of checkpoints as a 4 GB one, spaced
/// further apart.
pub const MAX_CHECKPOINTS: usize = 65_536;

/// The channel depth between the index task and the UI.
pub const INDEX_CHANNEL_DEPTH: usize = 32;

/// The smallest `viewer.index_chunk` that is honoured. Below this the task
/// would spend all its time sending messages.
pub const MIN_CHUNK: u64 = 4 * 1024;

/// A point the index can restart a scan from (the checkpoint).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Checkpoint {
    /// The byte offset.
    pub offset: u64,
    /// The 0-based number of the line that *contains* `offset`.
    pub line: u64,
    /// True when `offset` is not the start of a line - a checkpoint forced
    /// inside a line longer than the chunk, so a lookup by offset still has
    /// somewhere bounded to start from. A lookup by line number skips these.
    pub mid_line: bool,
}

impl Checkpoint {
    /// The checkpoint every index starts with: byte 0 is line 0's start.
    pub const ORIGIN: Self = Self {
        offset: 0,
        line: 0,
        mid_line: false,
    };
}

/// One `index_chunk` step's findings, on its way from the background task to
/// the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexBatch {
    /// Which viewer asked. A batch for a viewer that has been closed, or for a
    /// file that has been reopened, is dropped.
    pub id: super::ViewerId,
    /// Bytes scanned as of this batch, from the start of the file.
    pub scanned: u64,
    /// Line starts found as of this batch.
    pub lines: u64,
    /// New checkpoints, ascending, all beyond every checkpoint sent before.
    pub checkpoints: Vec<Checkpoint>,
    /// True when the scan reached the end of the file.
    pub done: bool,
    /// True when the last byte scanned was a `\n`, so the file's final line is
    /// not a partial one.
    pub ends_with_newline: bool,
    /// Set when the scan stopped on a read error. The index keeps what it had
    /// and stops being able to complete (the spirit: a failure part
    /// way through does not close the file).
    pub error: Option<String>,
}

/// What the background scan has found so far.
#[derive(Debug, Clone)]
pub struct LineIndex {
    checkpoints: Vec<Checkpoint>,
    /// Checkpoints are at least this far apart. Doubles on each decimation.
    spacing: u64,
    scanned: u64,
    lines: u64,
    complete: bool,
    ends_with_newline: bool,
    error: Option<String>,
}

impl LineIndex {
    /// An index that has found nothing yet, over a file whose scan will step in
    /// `chunk`-byte checkpoints.
    pub fn new(chunk: u64) -> Self {
        Self {
            checkpoints: vec![Checkpoint::ORIGIN],
            spacing: chunk.max(MIN_CHUNK),
            scanned: 0,
            lines: 0,
            complete: false,
            ends_with_newline: true,
            error: None,
        }
    }

    /// Bytes scanned so far. Everything below this offset is known exactly;
    /// everything above it is a guess (the "approximate").
    pub const fn scanned(&self) -> u64 {
        self.scanned
    }

    /// Line starts found so far.
    pub const fn lines(&self) -> u64 {
        self.lines
    }

    /// True once the scan has reached the end of the file. Until then, `End`
    /// and percentage seeks are approximate and say so.
    pub const fn is_complete(&self) -> bool {
        self.complete
    }

    /// True when the scanned region ends on a `\n`.
    pub const fn ends_with_newline(&self) -> bool {
        self.ends_with_newline
    }

    /// Why the scan stopped early, if it did.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// The current checkpoint spacing - how far a lookup may have to scan.
    pub const fn spacing(&self) -> u64 {
        self.spacing
    }

    /// Every checkpoint, ascending by offset.
    pub fn checkpoints(&self) -> &[Checkpoint] {
        &self.checkpoints
    }

    /// How many lines the file is *known* to have.
    ///
    /// A file whose scanned region does not end on a `\n` has one more line
    /// started but not finished, and that partial line counts - it is a line
    /// you can put the cursor on.
    pub fn known_lines(&self) -> u64 {
        if self.ends_with_newline {
            self.lines
        } else {
            self.lines.saturating_add(1)
        }
    }

    /// The last checkpoint at or before `offset`. Never fails: the origin is
    /// always present.
    pub fn checkpoint_for_offset(&self, offset: u64) -> Checkpoint {
        let idx = self.checkpoints.partition_point(|c| c.offset <= offset);
        self.checkpoints
            .get(idx.saturating_sub(1))
            .copied()
            .unwrap_or(Checkpoint::ORIGIN)
    }

    /// The last **line-start** checkpoint at or before line `line`.
    ///
    /// Mid-line checkpoints are skipped: they do not answer "where does line
    /// *n* begin".
    pub fn checkpoint_for_line(&self, line: u64) -> Checkpoint {
        let mut best = Checkpoint::ORIGIN;
        // Checkpoints are ascending in `line` as well as in `offset`, so the
        // partition point is exact and the walk back is only over the mid-line
        // run immediately below it.
        let idx = self.checkpoints.partition_point(|c| c.line <= line);
        for c in self.checkpoints.get(..idx).unwrap_or(&[]).iter().rev() {
            if !c.mid_line && c.line <= line {
                best = *c;
                break;
            }
        }
        best
    }

    /// Fold one background batch in.
    ///
    /// Batches arrive in order and each carries only what is new, so this is
    /// append-and-maybe-decimate. A batch that has fallen behind the index -
    /// which cannot happen for one viewer, but would if two scans were ever
    /// running - is ignored rather than allowed to move the index backwards.
    pub fn apply(&mut self, batch: &IndexBatch) {
        if batch.scanned < self.scanned {
            return;
        }
        self.scanned = batch.scanned;
        self.lines = batch.lines;
        self.ends_with_newline = batch.ends_with_newline;
        if batch.done {
            self.complete = true;
        }
        if batch.error.is_some() {
            self.error.clone_from(&batch.error);
        }
        let last = self.checkpoints.last().map_or(0, |c| c.offset);
        self.checkpoints.extend(
            batch
                .checkpoints
                .iter()
                .copied()
                .filter(|c| c.offset > last),
        );
        self.decimate();
    }

    /// Halve the checkpoint list and double the spacing, until the list fits.
    ///
    /// This is what makes the index's memory a constant rather than a function
    /// of file size. The origin is index 0 and survives every
    /// pass, so `checkpoint_for_*` always has an answer.
    fn decimate(&mut self) {
        while self.checkpoints.len() > MAX_CHECKPOINTS {
            let mut keep = 0_usize;
            self.checkpoints.retain(|_| {
                let take = keep.is_multiple_of(2);
                keep = keep.saturating_add(1);
                take
            });
            self.spacing = self.spacing.saturating_mul(2);
        }
    }
}

/// The background scan.
///
/// Reads the file forward in `chunk`-byte windows, counting `\n` and dropping a
/// checkpoint at each chunk boundary. It never holds more than one window, it
/// checks `cancel` between windows so closing the viewer stops it promptly, and
/// it stops the moment its channel is closed.
///
/// Blocking on purpose: this is `spawn_blocking` work, not async work. A `read`
/// on a 40 GB file is the thing being done, and doing it on a blocking-pool
/// thread is what keeps it off the frame path.
pub fn scan(
    id: super::ViewerId,
    mut source: Source,
    chunk: u64,
    cancel: Arc<AtomicBool>,
    tx: mpsc::Sender<IndexBatch>,
) {
    let chunk = chunk.max(MIN_CHUNK);
    let step = WindowLen::new(usize::try_from(chunk).unwrap_or(usize::MAX));
    let mut scanned = 0_u64;
    let mut lines = 0_u64;
    let mut ends_with_newline = true;
    // The next offset at which a checkpoint is owed.
    let mut next_checkpoint = chunk;

    loop {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        let window = match source.read_window(scanned, step) {
            Ok(w) => w,
            Err(err) => {
                let _ = tx.blocking_send(IndexBatch {
                    id,
                    scanned,
                    lines,
                    checkpoints: Vec::new(),
                    done: false,
                    ends_with_newline,
                    error: Some(err.to_string()),
                });
                return;
            }
        };
        let bytes = window.bytes();
        let base = window.at();
        let mut checkpoints = Vec::new();
        // A checkpoint is owed at `next_checkpoint`; it is placed at the first
        // line start at or after it, so it names a real line boundary. If the
        // chunk holds no line start beyond the boundary, one is forced at the
        // boundary itself and flagged mid-line, which bounds the scan a lookup
        // by offset has to do even on a file with no newlines at all.
        let mut owed = next_checkpoint;
        let mut placed_for_owed = false;
        for (i, b) in bytes.iter().enumerate() {
            let at = base.saturating_add(i as u64);
            if *b != b'\n' {
                continue;
            }
            lines = lines.saturating_add(1);
            let start = at.saturating_add(1);
            if start >= owed && !placed_for_owed {
                checkpoints.push(Checkpoint {
                    offset: start,
                    line: lines,
                    mid_line: false,
                });
                placed_for_owed = true;
            }
        }
        let end = window.end();
        while owed < end {
            if !placed_for_owed {
                checkpoints.push(Checkpoint {
                    offset: owed,
                    line: lines,
                    mid_line: true,
                });
            }
            owed = owed.saturating_add(chunk);
            placed_for_owed = false;
        }
        next_checkpoint = owed;
        if let Some(last) = bytes.last() {
            ends_with_newline = *last == b'\n';
        }
        scanned = end;
        let done = window.hit_eof();
        let batch = IndexBatch {
            id,
            scanned,
            lines,
            checkpoints,
            done,
            ends_with_newline,
            error: None,
        };
        if tx.blocking_send(batch).is_err() {
            // Nobody is listening: the viewer was closed.
            return;
        }
        if done {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn batch(scanned: u64, lines: u64, cps: Vec<Checkpoint>, done: bool) -> IndexBatch {
        IndexBatch {
            id: super::super::ViewerId(1),
            scanned,
            lines,
            checkpoints: cps,
            done,
            ends_with_newline: true,
            error: None,
        }
    }

    #[test]
    fn an_empty_index_still_answers_every_lookup() {
        let idx = LineIndex::new(1024);
        assert_eq!(idx.checkpoint_for_offset(0), Checkpoint::ORIGIN);
        assert_eq!(idx.checkpoint_for_offset(u64::MAX), Checkpoint::ORIGIN);
        assert_eq!(idx.checkpoint_for_line(9_999), Checkpoint::ORIGIN);
        assert!(!idx.is_complete());
    }

    #[test]
    fn lookups_land_on_the_checkpoint_below() {
        let mut idx = LineIndex::new(MIN_CHUNK);
        idx.apply(&batch(
            30_000,
            300,
            vec![
                Checkpoint {
                    offset: 4_100,
                    line: 100,
                    mid_line: false,
                },
                Checkpoint {
                    offset: 8_200,
                    line: 200,
                    mid_line: false,
                },
            ],
            false,
        ));
        assert_eq!(idx.checkpoint_for_offset(0).offset, 0);
        assert_eq!(idx.checkpoint_for_offset(4_099).offset, 0);
        assert_eq!(idx.checkpoint_for_offset(4_100).offset, 4_100);
        assert_eq!(idx.checkpoint_for_offset(9_999).offset, 8_200);
        assert_eq!(idx.checkpoint_for_line(150).line, 100);
        assert_eq!(idx.checkpoint_for_line(200).line, 200);
        assert_eq!(idx.checkpoint_for_line(1).line, 0);
    }

    #[test]
    fn a_mid_line_checkpoint_is_never_the_answer_to_a_line_lookup() {
        let mut idx = LineIndex::new(MIN_CHUNK);
        idx.apply(&batch(
            30_000,
            5,
            vec![
                Checkpoint {
                    offset: 4_096,
                    line: 3,
                    mid_line: true,
                },
                Checkpoint {
                    offset: 8_192,
                    line: 3,
                    mid_line: true,
                },
            ],
            false,
        ));
        // By offset it is exactly what bounds the scan...
        assert_eq!(idx.checkpoint_for_offset(9_000).offset, 8_192);
        // ...and by line it is not an answer, because it is not a line start.
        assert_eq!(idx.checkpoint_for_line(3), Checkpoint::ORIGIN);
    }

    #[test]
    fn the_checkpoint_list_is_capped_however_big_the_file_is() {
        let mut idx = LineIndex::new(MIN_CHUNK);
        let start_spacing = idx.spacing();
        // Four times the cap, arriving in batches, as the real scan sends them.
        let total = MAX_CHECKPOINTS * 4;
        for step in 0..8 {
            let cps: Vec<Checkpoint> = (0..total / 8)
                .map(|i| {
                    let n = (step * (total / 8) + i + 1) as u64;
                    Checkpoint {
                        offset: n * MIN_CHUNK,
                        line: n,
                        mid_line: false,
                    }
                })
                .collect();
            idx.apply(&batch(0, 0, cps, false));
        }
        assert!(
            idx.checkpoints().len() <= MAX_CHECKPOINTS,
            "{} checkpoints kept",
            idx.checkpoints().len()
        );
        assert!(
            idx.spacing() > start_spacing,
            "decimation widens the spacing rather than losing the coverage"
        );
        // And it is still monotonic and still usable.
        assert!(idx.checkpoints().windows(2).all(|w| match w {
            [a, b] => a.offset < b.offset,
            _ => true,
        }));
        assert_eq!(idx.checkpoints().first().copied(), Some(Checkpoint::ORIGIN));
    }

    #[tokio::test]
    async fn a_scan_finds_the_lines_and_finishes() {
        let text = b"one\ntwo\nthree\nfour\n".repeat(500);
        let source = Source::from_memory(text.clone()).expect("open");
        let (tx, mut rx) = mpsc::channel(INDEX_CHANNEL_DEPTH);
        let cancel = Arc::new(AtomicBool::new(false));
        let handle = tokio::task::spawn_blocking(move || {
            scan(super::super::ViewerId(1), source, MIN_CHUNK, cancel, tx)
        });

        let mut idx = LineIndex::new(MIN_CHUNK);
        while let Some(b) = rx.recv().await {
            idx.apply(&b);
        }
        handle.await.expect("task");

        assert!(idx.is_complete());
        assert_eq!(idx.scanned(), text.len() as u64);
        assert_eq!(idx.lines(), 2_000);
        assert!(idx.ends_with_newline());
        assert_eq!(idx.known_lines(), 2_000);
        assert!(idx.error().is_none());
        assert!(
            idx.checkpoints().len() > 1,
            "a file bigger than one chunk gets checkpoints"
        );
    }

    #[tokio::test]
    async fn a_file_with_no_newline_at_all_still_gets_bounded_checkpoints() {
        let text = vec![b'x'; (MIN_CHUNK * 4) as usize];
        let source = Source::from_memory(text).expect("open");
        let (tx, mut rx) = mpsc::channel(INDEX_CHANNEL_DEPTH);
        let cancel = Arc::new(AtomicBool::new(false));
        tokio::task::spawn_blocking(move || {
            scan(super::super::ViewerId(2), source, MIN_CHUNK, cancel, tx)
        })
        .await
        .expect("task");

        let mut idx = LineIndex::new(MIN_CHUNK);
        while let Some(b) = rx.recv().await {
            idx.apply(&b);
        }
        assert_eq!(idx.lines(), 0);
        assert!(!idx.ends_with_newline());
        assert_eq!(idx.known_lines(), 1, "one very long line is still one line");
        // Every gap between checkpoints is bounded, which is the point.
        for w in idx.checkpoints().windows(2) {
            if let [a, b] = w {
                assert!(b.offset - a.offset <= idx.spacing(), "{a:?} -> {b:?}");
            }
        }
    }

    #[tokio::test]
    async fn cancelling_before_the_scan_starts_sends_nothing() {
        let source = Source::from_memory(vec![b'\n'; (MIN_CHUNK * 64) as usize]).expect("open");
        let (tx, mut rx) = mpsc::channel(INDEX_CHANNEL_DEPTH);
        let cancel = Arc::new(AtomicBool::new(true));
        tokio::task::spawn_blocking(move || {
            scan(super::super::ViewerId(3), source, MIN_CHUNK, cancel, tx)
        })
        .await
        .expect("task");
        assert!(rx.recv().await.is_none(), "nothing was ever sent");
    }

    /// The half that matters on a 40 GB file: the flag goes up while the scan
    /// is already running, and the loop has to see it between windows.
    ///
    /// A scanner that reads the flag once on the way in and never again passes
    /// the test above and leaves the viewer waiting here, so this one cancels
    /// only after a batch has arrived - the scan is provably running - and
    /// asserts it stopped well short of the end.
    ///
    /// The bound is not a race. The channel holds `INDEX_CHANNEL_DEPTH`
    /// batches and the scan blocks once it is full, so no more than that many
    /// windows plus the one in flight can have been scanned before the flag
    /// was noticed, whatever the two threads do.
    #[tokio::test]
    async fn cancelling_mid_scan_stops_it_well_before_the_end() {
        const CHUNKS: u64 = 512;
        let len = MIN_CHUNK.saturating_mul(CHUNKS);
        let source = Source::from_memory(vec![b'\n'; len as usize]).expect("open");
        let (tx, mut rx) = mpsc::channel(INDEX_CHANNEL_DEPTH);
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&cancel);
        let handle = tokio::task::spawn_blocking(move || {
            scan(super::super::ViewerId(4), source, MIN_CHUNK, cancel, tx)
        });

        let mut idx = LineIndex::new(MIN_CHUNK);
        let first = rx.recv().await.expect("the first window was reported");
        idx.apply(&first);
        flag.store(true, Ordering::Relaxed);
        let mut batches = 1_usize;
        while let Some(b) = rx.recv().await {
            idx.apply(&b);
            batches = batches.saturating_add(1);
        }
        handle.await.expect("task");

        let ceiling = INDEX_CHANNEL_DEPTH.saturating_add(2);
        assert!(
            batches <= ceiling,
            "{batches} batches for a {CHUNKS}-chunk file: the scan did not \
             stop when the flag went up"
        );
        assert!(
            idx.scanned() < len,
            "it scanned the whole {len}-byte file anyway"
        );
        assert!(!idx.is_complete(), "a cancelled scan never reports done");
    }
}
