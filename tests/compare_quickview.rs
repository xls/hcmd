//! the `Shift+F2` and the `Ctrl+Q`, driven the way the
//! event loop drives them: state on a headless [`App`], no terminal anywhere.
//!
//! The four-step rule itself is unit-tested in `crate::ops::compare`; what is
//! here is the half that needs an application - two panels with marks, a job
//! that is queued rather than run, a debounce that is armed by a cursor and
//! answered by the event loop, and a directory that goes through the same size
//! cache `Ctrl+L` fills.
//!
//! Invariants I7 to I10 of the design are each asserted here, by
//! name.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "an integration test is its own crate, so it is not #[cfg(test)] \
              and clippy.toml's allow-*-in-tests keys do not reach it. \
              Panicking assertions are the point of a test."
)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant, SystemTime};

use holoscommander::app::App;
use holoscommander::config::{Config, Keymap, Theme};
use holoscommander::ops::walk::TreeStats;
use holoscommander::ops::{JobContext, JobKind, JobSpec};
use holoscommander::panel::Side;
use holoscommander::ui::quickview::DirSummary;
use holoscommander::vfs::{Entry, LocalFs, VfsPath};

/// Distinguishes the fixture directories of tests that run in parallel.
static NEXT: AtomicU32 = AtomicU32::new(0);

/// A directory of real files, removed on drop.
///
/// Quick view opens files and the contents job reads them, so both need
/// something on disk; the comparison rule itself needs nothing at all and is
/// tested without one.
struct Tree {
    root: PathBuf,
}

impl Tree {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "hcmd-compare-{}-{}-{tag}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("fixture root");
        Self { root }
    }

    fn file(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.root.join(name);
        fs::write(&path, bytes).expect("fixture file");
        path
    }

    fn dir(&self, name: &str) -> PathBuf {
        let path = self.root.join(name);
        fs::create_dir_all(&path).expect("fixture dir");
        path
    }

    fn root(&self) -> &Path {
        &self.root
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn app_with(config: Config) -> App {
    App::headless(config, Keymap::builtin(), Theme::blue())
}

/// Put `entries` into one panel, sitting at `path`, with the listing finished.
fn seed(app: &mut App, side: Side, path: &Path, entries: Vec<Entry>) {
    app.navigate(side, VfsPath::local(path));
    let _ = app.take_pending_reads();
    let tab = app.panel_mut(side).active_tab_mut();
    tab.entries = entries;
    tab.loading = false;
    tab.cursor = 0;
}

fn file(name: &str, size: u64, mtime_secs: u64) -> Entry {
    Entry {
        size,
        mtime: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(mtime_secs)),
        ..Entry::file(name)
    }
}

fn marks(app: &App, side: Side) -> Vec<String> {
    let mut out: Vec<String> = app.panel(side).active_tab().marks.iter().cloned().collect();
    out.sort();
    out
}

// ---------------------------------------------------------------- compare ---

/// I7, at the application level: `Shift+F2` marks the four-step rule's answer
/// in **both** panels and nothing else, and touches nothing else at all.
#[test]
fn compare_marks_both_panels_and_leaves_everything_else_alone() {
    let mut app = app_with(Config::default());
    seed(
        &mut app,
        Side::Left,
        Path::new("/a"),
        vec![
            Entry::parent_entry(),
            file("same", 10, 100),
            file("bigger", 10, 100),
            file("older", 10, 100),
            file("left-only", 10, 100),
            Entry::dir("shared"),
        ],
    );
    seed(
        &mut app,
        Side::Right,
        Path::new("/b"),
        vec![
            Entry::parent_entry(),
            file("same", 10, 101),
            file("bigger", 20, 100),
            file("older", 10, 9_000),
            file("right-only", 10, 100),
            Entry::dir("shared"),
        ],
    );
    app.left.active_tab_mut().cursor = 3;
    app.right.active_tab_mut().cursor = 2;

    app.compare_lists();

    assert_eq!(marks(&app, Side::Left), ["bigger", "left-only", "older"]);
    assert_eq!(marks(&app, Side::Right), ["bigger", "older", "right-only"]);
    assert_eq!(
        app.left.active_tab().cursor,
        3,
        "it does not touch the cursor"
    );
    assert_eq!(app.right.active_tab().cursor, 2);
    assert!(!app.dialog_is_open(), "it opens no window");
    let message = app.message.clone().unwrap_or_default();
    assert_eq!(message, "4 differ of 6 compared", "{message}");
    assert!(
        app.take_pending_jobs().is_empty(),
        "contents are not read unless ops.compare_contents is on"
    );
}

/// A second `Shift+F2` after files have been made identical clears what the
/// first one marked: the design marks the differences "and nothing else",
/// which means the marks are replaced rather than added to.
#[test]
fn a_second_compare_replaces_the_first_ones_marks() {
    let mut app = app_with(Config::default());
    let left = vec![file("a", 1, 1), file("b", 2, 1)];
    let right = vec![file("a", 9, 1), file("b", 2, 1)];
    seed(&mut app, Side::Left, Path::new("/a"), left);
    seed(&mut app, Side::Right, Path::new("/b"), right);
    app.compare_lists();
    assert_eq!(marks(&app, Side::Left), ["a"]);

    // The panels now agree.
    app.right.active_tab_mut().entries = vec![file("a", 1, 1), file("b", 2, 1)];
    app.compare_lists();
    assert!(marks(&app, Side::Left).is_empty());
    assert!(marks(&app, Side::Right).is_empty());
    assert_eq!(
        app.message.as_deref(),
        Some("the two lists are identical"),
        "and it says so"
    );
}

/// with `ops.compare_contents` on, the pairs the first three
/// steps could not separate become **one job**, positionally paired.
#[test]
fn the_contents_option_queues_one_job_over_the_undecided_pairs() {
    let mut config = Config::default();
    config.ops.compare_contents = true;
    let mut app = app_with(config);
    seed(
        &mut app,
        Side::Left,
        Path::new("/a"),
        vec![
            file("same", 10, 100),
            file("also-same", 4, 100),
            file("bigger", 10, 100),
            Entry::dir("shared"),
        ],
    );
    seed(
        &mut app,
        Side::Right,
        Path::new("/b"),
        vec![
            file("same", 10, 100),
            file("also-same", 4, 100),
            file("bigger", 20, 100),
            Entry::dir("shared"),
        ],
    );

    app.compare_lists();
    assert_eq!(
        marks(&app, Side::Left),
        ["bigger"],
        "steps 1 to 3 still mark in place, without reading a byte"
    );

    let queued = app.take_pending_jobs();
    assert_eq!(queued.len(), 1, "one job, not one per pair");
    let Some(request) = queued.first() else {
        panic!("a job was queued");
    };
    assert_eq!(request.spec.kind, JobKind::Compare);
    assert_eq!(
        request.spec.sources,
        vec![VfsPath::local("/a/also-same"), VfsPath::local("/a/same"),]
    );
    assert_eq!(
        request.spec.targets,
        vec![VfsPath::local("/b/also-same"), VfsPath::local("/b/same"),],
        "the pairs are positional, exactly as a rename's are"
    );
    assert!(
        !request
            .spec
            .sources
            .iter()
            .any(|p| p.to_string().contains("shared")),
        "a directory is never sent to the contents job"
    );
}

/// The worker reads both sides and hands back the names that differ, and
/// [`App::finish_compare`] folds them into both panels' marks.
#[test]
fn the_contents_job_names_what_differs_and_both_panels_mark_it() {
    let tree = Tree::new("contents");
    let a = tree.dir("a");
    let b = tree.dir("b");
    fs::write(a.join("same"), b"hello").expect("fixture");
    fs::write(b.join("same"), b"hello").expect("fixture");
    fs::write(a.join("changed"), b"hello").expect("fixture");
    fs::write(b.join("changed"), b"world").expect("fixture");

    let spec = JobSpec::compare(vec![
        (
            VfsPath::local(a.join("same")),
            VfsPath::local(b.join("same")),
        ),
        (
            VfsPath::local(a.join("changed")),
            VfsPath::local(b.join("changed")),
        ),
    ]);
    let (mut ctx, _rx, _decisions, _cancel) = JobContext::for_test(JobKind::Compare);
    holoscommander::ops::run(&LocalFs::new(), &spec, &mut ctx);
    let summary = ctx.finish();
    assert_eq!(summary.differing, vec!["changed".to_string()]);
    assert!(summary.failures.is_empty(), "{:?}", summary.failures);
    assert_eq!(summary.files_done, 2, "both pairs were compared");

    let mut app = app_with(Config::default());
    seed(
        &mut app,
        Side::Left,
        &a,
        vec![file("same", 5, 1), file("changed", 5, 1)],
    );
    seed(
        &mut app,
        Side::Right,
        &b,
        vec![file("same", 5, 1), file("changed", 5, 1)],
    );
    app.finish_compare(&summary);
    assert_eq!(marks(&app, Side::Left), ["changed"]);
    assert_eq!(marks(&app, Side::Right), ["changed"]);
    assert_eq!(app.message.as_deref(), Some("1 differ by contents"));
}

// ------------------------------------------------------------- quick view ---

/// I8: any number of cursor moves within `viewer.quick_view_delay` open **at
/// most one** viewer, and it is the one the cursor came to rest on.
#[test]
fn two_hundred_cursor_moves_open_exactly_one_viewer() {
    let tree = Tree::new("debounce");
    let names: Vec<String> = (0..200).map(|n| format!("file-{n:03}.txt")).collect();
    for name in &names {
        tree.file(name, b"contents\n");
    }
    let entries: Vec<Entry> = names.iter().map(|n| file(n, 9, 1)).collect();

    let mut app = app_with(Config::default());
    seed(&mut app, Side::Left, tree.root(), entries);
    app.quick_view_toggle();
    assert_eq!(app.quick_view_side(), Some(Side::Right));

    // Held `Down`: the cursor moves, the pending file is replaced, and the
    // event loop gets its turn between every keystroke.
    for row in 0..200 {
        app.left.active_tab_mut().cursor = row;
        app.note_quick_view_cursor();
        app.service_quick_view(Instant::now());
        assert!(
            app.quick_viewer().is_none(),
            "nothing opens while the cursor is still moving"
        );
    }

    // The cursor stops. Only now is anything read.
    expire_debounce(&mut app);
    app.service_quick_view(Instant::now());
    let Some(viewer) = app.quick_viewer() else {
        panic!("the file the cursor came to rest on is opened");
    };
    assert!(
        viewer.title().ends_with("file-199.txt"),
        "and it is the last one, not the first: {}",
        viewer.title()
    );
}

/// I9: the panel showing the quick view keeps its own path and cursor, and
/// `Ctrl+Q` again gives it its listing back.
#[test]
fn the_showing_panel_keeps_its_path_and_cursor_and_ctrl_q_gives_it_back() {
    let tree = Tree::new("keeps");
    tree.file("a.txt", b"first line\nsecond line\n");
    let mut app = app_with(Config::default());
    seed(
        &mut app,
        Side::Left,
        tree.root(),
        vec![file("a.txt", 22, 1)],
    );
    seed(
        &mut app,
        Side::Right,
        Path::new("/elsewhere"),
        vec![file("kept", 1, 1), file("also-kept", 1, 1)],
    );
    app.right.active_tab_mut().cursor = 1;

    app.quick_view_toggle();
    expire_debounce(&mut app);
    app.service_quick_view(Instant::now());
    assert!(app.quick_viewer().is_some(), "the file opened");
    assert_eq!(
        app.right.active_tab().path,
        VfsPath::local("/elsewhere"),
        "the panel keeps its path while it is showing a file"
    );
    assert_eq!(app.right.active_tab().cursor, 1, "and its cursor");
    assert_eq!(app.right.active_tab().entries.len(), 2, "and its listing");

    app.quick_view_toggle();
    assert_eq!(app.quick_view_side(), None, "Ctrl+Q again closes it");
    assert!(app.quick_viewer().is_none());
}

/// I10: a directory under the cursor goes through [`App::request_size`] and
/// the shared size cache; a directory already sized shows instantly and asks
/// for no walk at all.
#[test]
fn a_directory_reuses_the_ctrl_l_size_walk() {
    let tree = Tree::new("dir");
    tree.dir("sub");
    let mut app = app_with(Config::default());
    seed(&mut app, Side::Left, tree.root(), vec![Entry::dir("sub")]);
    app.quick_view_toggle();
    expire_debounce(&mut app);
    app.service_quick_view(Instant::now());

    let Some(quick) = app.quick.as_deref() else {
        panic!("a quick view is showing");
    };
    assert_eq!(quick.summary, Some(DirSummary::Walking));
    assert!(
        quick.viewer.is_none(),
        "a directory is not opened as a file"
    );
    let queued = app.take_pending_jobs();
    assert_eq!(queued.len(), 1);
    let Some(request) = queued.first() else {
        panic!("a walk was queued");
    };
    assert_eq!(
        request.spec.kind,
        JobKind::Size,
        "the same walk Ctrl+L and Space use, not a second kind"
    );

    // The walk finishes into the shared cache; the next frame picks it up.
    let stats = TreeStats {
        bytes: 4096,
        files: 3,
        dirs: 1,
    };
    app.jobs
        .sizes
        .insert(VfsPath::local(tree.root().join("sub")), stats);
    app.service_quick_view(Instant::now());
    let Some(quick) = app.quick.as_deref() else {
        panic!("a quick view is showing");
    };
    assert_eq!(quick.summary, Some(DirSummary::Done(stats)));

    // And a directory that is already in the cache costs nothing: the cursor
    // leaves and comes back, and no walk is asked for.
    app.left.active_tab_mut().cursor = 0;
    app.note_quick_view_cursor();
    if let Some(quick) = app.quick.as_deref_mut() {
        quick.subject = None;
    }
    expire_debounce(&mut app);
    app.service_quick_view(Instant::now());
    assert!(
        app.take_pending_jobs().is_empty(),
        "a directory already sized is free"
    );
}

/// An unreadable file leaves its reason in the quick view rather than raising
/// a dialog, which is what the debounce exists to make safe.
#[test]
fn a_file_that_cannot_be_opened_reports_in_place() {
    let tree = Tree::new("missing");
    let mut app = app_with(Config::default());
    // A row for a file that is not there: the listing said so and the file has
    // since gone, which is the ordinary race.
    seed(
        &mut app,
        Side::Left,
        tree.root(),
        vec![file("gone.txt", 1, 1)],
    );
    app.quick_view_toggle();
    expire_debounce(&mut app);
    app.service_quick_view(Instant::now());

    let Some(quick) = app.quick.as_deref() else {
        panic!("a quick view is showing");
    };
    assert!(quick.viewer.is_none());
    assert!(quick.error.is_some(), "the reason is kept");
    assert!(!app.dialog_is_open(), "and it is not a dialog");
}

/// Move the debounce's clock into the past, which is what
/// [`App::quick_view_deadline`] exists to make testable without one.
/// "`Tab` moves focus into it, after which the viewer's own
/// keys apply" - and the resolution of that against
/// `Focus`: no new `Focus` variant and no new `KeyContext`, one branch in
/// `dispatch`.
///
/// Four keys stay the panel's, because a quick view is a panel showing a file
/// and there has to be a way out: `Tab`, `Ctrl+Q`, `F1` and `F10`. Everything
/// else goes to the viewer.
#[test]
fn tab_moves_focus_into_the_quick_view_and_the_viewers_keys_then_apply() {
    use holoscommander::input::{Focus, KeyCode, KeyEvent, KeyModifiers, dispatch};

    let tree = Tree::new("focus");
    tree.file("a.txt", b"one\ntwo\nthree\nfour\nfive\n");
    let mut app = app_with(Config::default());
    seed(
        &mut app,
        Side::Left,
        tree.root(),
        vec![file("a.txt", 24, 1)],
    );
    seed(
        &mut app,
        Side::Right,
        Path::new("/elsewhere"),
        vec![file("kept", 1, 1)],
    );

    app.quick_view_toggle();
    expire_debounce(&mut app);
    app.service_quick_view(Instant::now());
    assert_eq!(app.quick_view_side(), Some(Side::Right));

    let press = |app: &mut App, code, mods| {
        dispatch(app, KeyEvent::new(code, mods)).expect("dispatch never fails on a synthetic key");
    };

    // The focus state stays `Focus::Panel`, which is what makes the design's
    // "it is still that panel" literally true.
    press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
    assert_eq!(app.focus, Focus::Panel(Side::Right));
    assert_eq!(app.active_side, Side::Right);

    // A viewer key now acts on the quick viewer rather than on the panel.
    let before = app
        .quick_viewer()
        .map(holoscommander::viewer::Viewer::top)
        .unwrap_or_default();
    press(&mut app, KeyCode::Down, KeyModifiers::NONE);
    let after = app
        .quick_viewer()
        .map(holoscommander::viewer::Viewer::top)
        .unwrap_or_default();
    assert_ne!(before, after, "Down scrolled the viewer, not the listing");
    assert_eq!(
        app.right.active_tab().cursor,
        0,
        "and left the panel's own cursor alone"
    );

    // Tab goes back out, and Ctrl+Q closes the quick view from inside it.
    press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
    assert_eq!(app.active_side, Side::Left);
    press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
    press(&mut app, KeyCode::Char('q'), KeyModifiers::CONTROL);
    assert_eq!(
        app.quick_view_side(),
        None,
        "Ctrl+Q reaches the panel even with the viewer focused"
    );
}

fn expire_debounce(app: &mut App) {
    let Some(quick) = app.quick.as_deref_mut() else {
        return;
    };
    let Some(pending) = quick.pending.as_mut() else {
        return;
    };
    let Some(earlier) = pending.at.checked_sub(Duration::from_secs(1)) else {
        return;
    };
    pending.at = earlier;
}

// ------------------------------------------------------------- rendering ---

/// nothing this milestone draws may panic or claim a rectangle it
/// does not have, at any terminal size or in either border mode.
///
/// The quick view is not a dialog, so the dialog
/// sweep does not cover it; it is a viewer drawn into a panel body, which is
/// the geometry most likely to run out of rows.
#[test]
fn a_quick_view_renders_at_every_size_in_both_border_modes() {
    let tree = Tree::new("render");
    tree.file("a.txt", b"one\ntwo\nthree\n");
    tree.dir("sub");

    for ascii in [false, true] {
        for (w, h) in [(200_u16, 50_u16), (80, 24), (60, 15), (20, 6)] {
            let mut config = Config::default();
            config.ui.ascii_borders = ascii;
            let mut app = app_with(config);
            seed(
                &mut app,
                Side::Left,
                tree.root(),
                vec![file("a.txt", 14, 1), Entry::dir("sub")],
            );
            app.quick_view_toggle();

            // A file, a directory being walked, and a file that is not there:
            // the three bodies the quick view can draw.
            for row in [0_usize, 1, 0] {
                app.left.active_tab_mut().cursor = row;
                app.note_quick_view_cursor();
                expire_debounce(&mut app);
                let area = ratatui::layout::Rect::new(0, 0, w, h);
                holoscommander::ui::sync_view_rows(&mut app, area);
                app.service_quick_view(Instant::now());
                let backend = ratatui::backend::TestBackend::new(w, h);
                let mut term = ratatui::Terminal::new(backend).expect("a test terminal");
                term.draw(|f| holoscommander::ui::draw(f, &app))
                    .expect("the frame is drawn");

                // Drawing outside the terminal panics `TestBackend`, so
                // getting here is the "it does not crash" half. What is on
                // screen is the other half, and it is the half a size the
                // quick view quietly gave up on would fail.
                let out = dump(term.backend().buffer());
                if w < 60 || h < 15 {
                    assert!(
                        out.contains("terminal too small"),
                        "ascii={ascii} {w}x{h} row={row}:\n{out}"
                    );
                } else if row == 1 {
                    // The directory body: its own path in the header, whether
                    // or not the size walk has answered yet.
                    assert!(
                        out.contains("/sub"),
                        "ascii={ascii} {w}x{h}: no directory body:\n{out}"
                    );
                } else {
                    // The file body: numbered lines of the file itself.
                    assert!(
                        out.contains("1 one") && out.contains("3 three"),
                        "ascii={ascii} {w}x{h}: no file body:\n{out}"
                    );
                }
                if ascii {
                    assert!(
                        out.is_ascii(),
                        "ascii={ascii} {w}x{h} row={row}: a non-ASCII glyph:\n{out}"
                    );
                }
                if let Some(quick) = app.quick.as_deref_mut() {
                    // Force the next row to be treated as a fresh subject
                    // rather than as the one already showing.
                    quick.subject = None;
                }
            }
        }
    }
}

/// Every cell of a rendered frame, row by row.
fn dump(buf: &ratatui::buffer::Buffer) -> String {
    let area = *buf.area();
    let mut out = String::new();
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            out.push_str(buf.cell((x, y)).map_or(" ", |c| c.symbol()));
        }
        out.push('\n');
    }
    out
}
