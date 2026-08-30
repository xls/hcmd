//! The event loop's own plumbing, driven from outside the binary.
//!
//! Everything here used to live in `src/main.rs` and was therefore reachable
//! only from the binary itself: the channels the loop creates, the tasks it
//! spawns onto them and the generation checks that decide which of their
//! answers still count. `holoscommander::runtime` is that layer as a library
//! module, and this file is the proof - each test creates the channels the
//! loop would have created, calls the loop's own step functions with them, and
//! reads back what arrived.
//!
//! No terminal is involved. The steps that need one - `apply_input` and
//! `drain_input`, which take a `Term` - are not exercised here; everything
//! between a queued request and the answer coming back over a channel is.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "an integration test is its own crate, so it is not #[cfg(test)] \
              and clippy.toml's allow-*-in-tests keys do not reach it. \
              Panicking assertions are the point of a test."
)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use holoscommander::app::reads::CapsEvent;
use holoscommander::app::{App, VfsEvent, ViewRequest};
use holoscommander::config::{Config, Keymap, Theme};
use holoscommander::input::Focus;
use holoscommander::panel::{Side, VirtualKind};
use holoscommander::runtime::{
    FinishedSearch, OpenedViewer, apply_vfs, open_pending_viewer, push_opened_viewer,
    service_search,
};
use holoscommander::search::Query;
use holoscommander::vfs::{Entry, VfsPath};
use holoscommander::viewer::index::IndexBatch;
use tokio::sync::mpsc;

/// Keeps concurrently running tests off each other's directory.
static NEXT: AtomicUsize = AtomicUsize::new(0);

/// How long a test waits for a channel that should already be answering.
///
/// Generous, because it is only ever paid when something is broken: every wait
/// here is on work that a healthy build finishes in milliseconds.
const PATIENCE: Duration = Duration::from_secs(20);

/// A throwaway directory, removed by [`Scratch::drop`].
struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Self {
        let n = NEXT.fetch_add(1, Ordering::SeqCst);
        let path =
            std::env::temp_dir().join(format!("hcmd-runtime-{}-{}-{n}", std::process::id(), name));
        std::fs::create_dir_all(&path).expect("a scratch directory");
        Self { path }
    }

    fn file(&self, name: &str, body: &str) -> PathBuf {
        let path = self.path.join(name);
        std::fs::write(&path, body).expect("wrote the fixture");
        path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn headless() -> App {
    App::headless(Config::default(), Keymap::builtin(), Theme::blue())
}

/// Wait for one message, failing the test rather than hanging the suite.
async fn next<T>(rx: &mut mpsc::Receiver<T>, what: &str) -> T {
    match tokio::time::timeout(PATIENCE, rx.recv()).await {
        Ok(Some(value)) => value,
        Ok(None) => panic!("the {what} channel closed with nothing on it"),
        Err(_) => panic!("nothing arrived on the {what} channel"),
    }
}

/// `F3` on a file: queued by `dispatch`, opened on the blocking pool, and back
/// over the loop's own channel.
///
/// The whole point of the seam. `open_pending_viewer` is handed the sender the
/// event loop would have handed it, and the test plays the part of the
/// `view_rx` arm: take what arrived, push it, and let the line index that
/// `push_opened_viewer` starts report over the index channel.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_queued_viewer_travels_through_the_view_channel() {
    let scratch = Scratch::new("view");
    let file = scratch.file("notes.txt", "one\ntwo\nthree\n");

    let mut app = headless();
    let (view_tx, mut view_rx) = mpsc::channel::<OpenedViewer>(4);
    let (index_tx, mut index_rx) = mpsc::channel::<IndexBatch>(64);
    let mut opening = false;

    app.request_view(ViewRequest::File {
        path: VfsPath::local(&file),
        at: None,
    });
    open_pending_viewer(&mut app, &view_tx, &index_tx, &mut opening);

    // The flag the loop swallows keystrokes on: a file viewer is being built
    // elsewhere, so the keys typed at it are held rather than given to the
    // panel behind it.
    assert!(opening, "a file open is in flight, so keys are being held");
    assert_eq!(app.viewer_depth(), 0, "nothing is on screen yet");

    let opened = next(&mut view_rx, "view").await;
    push_opened_viewer(&mut app, opened, &index_tx);

    assert_eq!(app.viewer_depth(), 1, "the viewer is on screen");
    assert_eq!(app.focus, Focus::Viewer);

    // And the scan `push_opened_viewer` put on the blocking pool reports into
    // the index channel, which is how the viewer learns how many lines it has.
    let mut done = None;
    while done.is_none() {
        let batch = next(&mut index_rx, "index").await;
        assert!(
            app.apply_index_batch(&batch),
            "the batch belongs to the viewer that is up"
        );
        if batch.done {
            done = Some(batch);
        }
    }
    let done = done.expect("the scan reached the end of the file");
    assert_eq!(done.lines, 3, "three lines in the fixture");
}

/// A generated page is text that is already in memory, so it never goes near
/// the channel - and the loop does not start holding keys for it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_generated_page_is_pushed_without_the_channel() {
    let mut app = headless();
    let (view_tx, mut view_rx) = mpsc::channel::<OpenedViewer>(4);
    let (index_tx, _index_rx) = mpsc::channel::<IndexBatch>(64);
    let mut opening = false;

    app.request_view(ViewRequest::Text {
        title: "About".to_string(),
        body: "a page that was generated\n".to_string(),
        help: false,
    });
    open_pending_viewer(&mut app, &view_tx, &index_tx, &mut opening);

    assert!(!opening, "nothing is in flight, so no key is held back");
    assert_eq!(app.viewer_depth(), 1, "it is up on this frame");
    assert!(
        view_rx.try_recv().is_err(),
        "a generated page costs the channel nothing"
    );
}

/// The `vfs_rx` arm: a batch fills the panel, a batch from a read the tab has
/// moved on from is dropped, and a completed listing asks its capability probe
/// over the channel the loop gave it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_listing_fills_the_panel_and_a_stale_batch_is_dropped() {
    let scratch = Scratch::new("read");
    scratch.file("a.txt", "");

    let mut app = headless();
    let (caps_tx, mut caps_rx) = mpsc::channel::<CapsEvent>(8);

    app.navigate(Side::Left, VfsPath::local(&scratch.path));
    let request = app
        .take_pending_reads()
        .pop()
        .expect("navigating asks for a read");
    let generation = request.generation;

    apply_vfs(
        &mut app,
        VfsEvent::Entries {
            side: Side::Left,
            tab: request.tab,
            generation,
            batch: vec![Entry::file("a.txt")],
        },
        &caps_tx,
    );
    assert_eq!(app.left.active_tab().entries.len(), 1);

    // A batch carrying a generation the tab has moved past is what a
    // superseded read keeps producing until its task notices it was aborted.
    apply_vfs(
        &mut app,
        VfsEvent::Entries {
            side: Side::Left,
            tab: request.tab,
            generation: generation + 1,
            batch: vec![Entry::file("stale.txt")],
        },
        &caps_tx,
    );
    assert_eq!(
        app.left.active_tab().entries.len(),
        1,
        "a stale batch never reaches the panel"
    );
    assert!(
        caps_rx.try_recv().is_err(),
        "and asks for no capability probe"
    );

    // `Done` is the one event that hands work back: the probe blocks, so the
    // loop spawns it and the answer arrives here.
    apply_vfs(
        &mut app,
        VfsEvent::Done {
            side: Side::Left,
            tab: request.tab,
            generation,
        },
        &caps_tx,
    );
    let caps = next(&mut caps_rx, "caps").await;
    assert_eq!(caps.generation, generation);
    assert_eq!(caps.side, Side::Left);
    app.apply_caps_event(caps);
}

/// The tally channel: a walk that has ended reports what it passed over, and
/// the report is what the loop turns into the one honest line at the end.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_finished_search_reports_its_tally_over_the_channel() {
    let scratch = Scratch::new("search");
    scratch.file("found.txt", "hello\n");

    let mut app = headless();
    let (tally_tx, mut tally_rx) = mpsc::channel::<FinishedSearch>(4);

    app.request_search(
        Query::new(VfsPath::local(&scratch.path)),
        VirtualKind::Branch,
    );
    service_search(&mut app, &tally_tx);

    let finished: FinishedSearch = next(&mut tally_rx, "tally").await;
    app.report_search_tally(&finished.started, &finished.tally);
}
