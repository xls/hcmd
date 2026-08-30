//! Shared fixtures and the tests for `App` itself.
//!
//! The unit tests live beside what they test; this file is what is left over
//! once each section of the app tree has taken its own with it, plus the
//! fixtures more than one of them needs.

use super::*;
use crate::ops::JobKind;
use crate::panel::{ColumnId, SortKey};
use crate::remote::auth::AuthPlan;
use crate::vfs::list::{ListSink, ListStatus};
use crate::vfs::{Entry, ListFs};

/// An application with one panel already listing `names`, which is the
/// starting point most of the tests in this tree need.
pub fn app_with(names: &[&str]) -> App {
    let mut app = App::headless(Config::default(), Keymap::builtin(), Theme::blue());
    let tab = app.left.active_tab_mut();
    tab.entries = names.iter().map(|n| Entry::file(*n)).collect();
    app
}

// ------------------------------ the design remote connections ---------

/// A target that is not a real host and never will be.
pub fn nas() -> Target {
    Target {
        protocol: crate::remote::Protocol::Sftp,
        host: "nas.local".to_string(),
        port: 2222,
        user: "thorin".to_string(),
        dir: Some("/srv".to_string()),
    }
}

/// Connect the active panel through the same route `Ctrl+F` uses, with an
/// in-memory transport instead of a socket.
///
/// Everything below the handshake is what these tests are about: the
/// dialog's answer becomes a queued request, the event loop starts it, and
/// a `RemoteEvent` puts the tab on the connection.
pub fn connect_active(app: &mut App) -> RemoteId {
    let transport = crate::remote::transport::FakeTransport::new()
        .with_dir("/srv")
        .with_file("/srv/a.txt", b"x");
    let fs = crate::remote::RemoteFs::new(
        nas(),
        Arc::new(transport) as Arc<dyn crate::remote::transport::RemoteTransport>,
        std::time::Duration::from_secs(2),
    );
    let id = app.remotes().register(fs).expect("register");
    app.connect_answered(Box::new(crate::dialog::ConnectAnswer {
        target: nas(),
        plan: AuthPlan::for_password_login(None),
        password: None,
        local_dir: None,
        hosts: None,
    }));
    let request = app.take_pending_connect().expect("the connect was queued");
    app.apply_remote_event(RemoteEvent::Connected {
        attempt: request.attempt,
        id,
        start: id.path("/srv"),
        saved: None,
    });
    id
}

// ------------------------------- the virtual listing ------

/// Point a panel at a virtual listing through the same code `Alt+F7` uses,
/// with no search engine behind it, and hand back its producer end.
///
/// Everything below the compile-and-spawn step is what the tests that use
/// this are about: the design makes the results a `ListFs` in the panel,
/// and the state machine of getting into and out of one has to hold whatever
/// the walk did or did not find.
pub fn show_listing(app: &mut App, side: Side, kind: VirtualKind, header: &str) -> ListSink {
    let tab_index = app.panel(side).active_index();
    let tab = app.panel(side).active_tab();
    // Exactly what `start_search` does: a search *from* a search keeps the
    // first one's origin, because a `list:` path is not a directory to
    // return to.
    let (origin, origin_cursor, previous) = match tab.virtual_view() {
        Some(view) => (
            view.origin.clone(),
            view.origin_cursor.clone(),
            Some(view.listing),
        ),
        None => (tab.path.clone(), tab.cursor_name(), None),
    };
    let (listing, sink) = ListFs::streaming(header, std::slice::from_ref(&origin));
    assert!(app.show_listing(
        side,
        tab_index,
        listing,
        PendingView {
            kind,
            header: header.to_string(),
            find: None,
            origin,
            origin_cursor,
            previous,
        }
    ));
    sink
}

#[test]
fn headless_needs_no_terminal_and_no_filesystem() {
    let app = App::headless(Config::default(), Keymap::builtin(), Theme::blue());
    assert_eq!(app.focus, Focus::Panel(Side::Left));
    assert_eq!(app.active_side, Side::Left);
    assert!(app.left.active_tab().entries.is_empty());
    assert!(!app.should_quit);
}

#[test]
fn the_active_side_survives_focus_moving_to_the_command_line() {
    let mut app = App::headless(Config::default(), Keymap::builtin(), Theme::blue());
    app.set_focus(Focus::Panel(Side::Right));
    app.set_focus(Focus::CommandLine);
    assert_eq!(app.active_side, Side::Right);
    assert_eq!(app.active_panel().side, Side::Right);
}

#[test]
fn navigation_queues_a_read_rather_than_touching_the_filesystem() {
    let mut app = App::headless(Config::default(), Keymap::builtin(), Theme::blue());
    app.navigate(Side::Left, VfsPath::local("/usr/share"));
    let reads = app.take_pending_reads();
    assert_eq!(reads.len(), 1);
    let first = reads.first().expect("one read");
    assert_eq!(first.side, Side::Left);
    assert_eq!(first.path, VfsPath::local("/usr/share"));
    assert!(app.left.active_tab().loading);
    assert!(app.take_pending_reads().is_empty());
}

#[test]
fn a_stale_listing_is_dropped() {
    let mut app = App::headless(Config::default(), Keymap::builtin(), Theme::blue());
    app.navigate(Side::Left, VfsPath::local("/a"));
    let stale = app.left.active_tab().generation;
    app.navigate(Side::Left, VfsPath::local("/b"));
    app.apply_vfs_event(VfsEvent::Entries {
        side: Side::Left,
        tab: 0,
        generation: stale,
        batch: vec![Entry::file("ghost")],
    });
    assert!(app.left.active_tab().entries.is_empty());
}

/// "results stream back over a channel as they are found",
/// and the channel is the ordinary directory-read one.
///
/// Reported from a real session against a remote: the status line counted
/// the hits while the panel showed none of them. The count is read
/// straight off the [`ListFs`]; the rows come through this function, which
/// held them until the listing ended. A local walk ends in milliseconds so
/// nobody saw it; a walk over a network does not.
///
/// No sleep and no stopwatch: the listing here is *deliberately* left
/// filling, which is the state a running walk is in, and the assertion is
/// that the row arrives anyway.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_row_found_reaches_the_panel_before_the_listing_ends() {
    let (listing, sink) = ListFs::streaming("[search: * in /root]", &[VfsPath::local("/root")]);
    let path = crate::vfs::list::ListingId(1).to_path();
    let (tx, mut rx) = mpsc::channel::<VfsEvent>(8);
    let reader = tokio::spawn(stream_read(
        listing as Arc<dyn Vfs>,
        ReadRequest {
            side: Side::Left,
            tab: 0,
            generation: 1,
            path,
        },
        tx,
    ));

    assert!(sink.push(Entry::file("hit.rs")), "the walk found one");
    let event = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .expect("a hit that is found is a hit that is shown, before the walk ends")
        .expect("the reader is still going");
    match event {
        VfsEvent::Entries { batch, .. } => {
            assert_eq!(
                batch.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
                vec!["hit.rs"]
            );
        }
        VfsEvent::Done { .. } | VfsEvent::Failed { .. } => {
            panic!("the row arrived as neither a row nor a batch")
        }
    }

    // And the listing still ends exactly once when the walk does.
    sink.finish(ListStatus::Complete);
    let ended = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while let Some(event) = rx.recv().await {
            if matches!(event, VfsEvent::Done { .. }) {
                return true;
            }
        }
        false
    })
    .await;
    assert_eq!(ended, Ok(true), "the read ends when the walk does");
    reader.await.expect("the reader finished");
}

/// Fill a tab the way a completed read would: entries, then `Done`.
/// Deliver one batch of rows to a tab and end the read, the way the event
/// loop does when a listing arrives.
pub fn deliver(app: &mut App, side: Side, names: &[&str]) {
    let generation = app.panel(side).active_tab().generation;
    app.apply_vfs_event(VfsEvent::Entries {
        side,
        tab: 0,
        generation,
        batch: names.iter().map(|n| Entry::file(*n)).collect(),
    });
    app.apply_vfs_event(VfsEvent::Done {
        side,
        tab: 0,
        generation,
    });
}

#[test]
fn a_reread_keeps_the_marks_and_the_cursor() {
    // selection is "preserved across re-reads where the path
    // still exists, cleared on directory change". A re-read is not a
    // directory change.
    let mut app = App::headless(Config::default(), Keymap::builtin(), Theme::blue());
    app.navigate(Side::Left, VfsPath::local("/x"));
    deliver(&mut app, Side::Left, &["a", "b", "c", "d"]);
    let tab = app.left.active_tab_mut();
    tab.move_to(2, 10);
    assert!(tab.toggle_mark());
    tab.move_to(1, 10);
    assert!(tab.toggle_mark());
    tab.move_to(3, 10);
    assert_eq!(tab.marks.len(), 2);

    app.reread(Side::Left);
    // The rows stay on screen while the read is in flight - clearing them
    // here was a visible flash of bare background after every copy. The
    // cursor is a hint until the listing lands, and the marks are
    // untouched.
    assert_eq!(
        app.left.active_tab().entries.len(),
        4,
        "the old rows are still drawn until the replacement arrives"
    );
    assert!(app.left.active_tab().replace_on_next_batch);
    app.left.active_tab_mut().clamp_cursor();
    assert_eq!(app.left.active_tab().cursor, 3);
    assert_eq!(app.left.active_tab().marks.len(), 2);

    // "b" vanished from the directory between the two reads; its mark goes
    // with it and "c" keeps its own.
    deliver(&mut app, Side::Left, &["a", "c", "d"]);
    let tab = app.left.active_tab();
    assert!(tab.marks.contains("c"));
    assert!(!tab.marks.contains("b"));
    assert_eq!(tab.marks.len(), 1);
    assert_eq!(tab.cursor, 2, "clamped into the shorter listing");
}

#[test]
fn a_reread_keeps_the_cursor_when_the_batch_arrives_unsorted() {
    // "`Ctrl+R` on a **normal panel refreshes** it, the same
    // as `F2`" - and the design keeps the cursor across a re-read.
    //
    // `a_reread_keeps_the_marks_and_the_cursor` above delivers names that
    // are already in sorted order, which makes the sort a no-op and hides
    // the case this covers: a backend streams its own `readdir` order, the
    // replacement batch lands under a cursor whose index means nothing any
    // more, and `Tab::sort_entries` re-anchors by reading the name at that
    // index. Before `App::reread` remembered the name, the cursor landed
    // on whatever row happened to sit there - a different one on each
    // press, since `readdir` order varies.
    let mut app = App::headless(Config::default(), Keymap::builtin(), Theme::blue());
    app.navigate(Side::Left, VfsPath::local("/x"));
    let unsorted = ["top.rs", "d", "a", "README.md", "b"];
    deliver(&mut app, Side::Left, &unsorted);
    assert_eq!(
        app.left
            .active_tab()
            .entries
            .iter()
            .map(|e| e.name.as_str())
            .collect::<Vec<_>>(),
        vec!["a", "b", "d", "README.md", "top.rs"],
        "the panel sorted what the backend streamed"
    );
    app.left.active_tab_mut().move_to(2, 10);
    assert_eq!(app.left.active_tab().cursor_name().as_deref(), Some("d"));

    app.reread(Side::Left);
    deliver(&mut app, Side::Left, &unsorted);
    assert_eq!(
        app.left.active_tab().cursor_name().as_deref(),
        Some("d"),
        "the refresh landed on the row the cursor was on"
    );
}

#[test]
fn a_reread_does_not_overrule_a_row_the_keystroke_already_asked_for() {
    // The other half of the rule above. `F7`, `F2`, `F4` and a paste all
    // name the row they expect to appear and then wait for the re-read
    // that produces it (`Tab::pending_select`). Their answer is about the
    // listing that is coming; the cursor's current name is about the one
    // being replaced, so the re-read only fills in when nothing is
    // pending.
    let mut app = App::headless(Config::default(), Keymap::builtin(), Theme::blue());
    app.navigate(Side::Left, VfsPath::local("/x"));
    deliver(&mut app, Side::Left, &["a", "b"]);
    app.left.active_tab_mut().pending_select = Some("mmmnew".to_string());

    app.reread(Side::Left);
    assert_eq!(
        app.left.active_tab().pending_select.as_deref(),
        Some("mmmnew"),
        "the created row still wins"
    );
    deliver(&mut app, Side::Left, &["a", "b", "mmmnew"]);
    assert_eq!(
        app.left.active_tab().cursor_name().as_deref(),
        Some("mmmnew")
    );
}

#[test]
fn a_filling_listing_does_not_re_sort_on_every_batch() {
    // the design puts an unbounded row count on the streaming read path,
    // and a full sort per 128-row batch is quadratic in it: a 275k-row
    // search sorted 2,149 times, which measured at tens of seconds of
    // event-loop work for an order one final sort produces in 0.2 s.
    //
    // `Tab::sort_streaming` sorts every batch of a small listing - which
    // is every real directory - and puts a large one on a doubling
    // schedule. The final order is the same either way, because
    // `VfsEvent::Done` sorts once more.
    let mut app = App::headless(Config::default(), Keymap::builtin(), Theme::blue());
    app.navigate(Side::Left, VfsPath::local("/x"));
    let generation = app.left.active_tab().generation;
    // Descending names, so an unsorted tail is impossible to mistake for a
    // sorted one.
    let rows = crate::panel::SORT_EVERY_BATCH_BELOW * 3;
    let mut names: Vec<String> = (0..rows).map(|i| format!("f{:06}", rows - i)).collect();
    for chunk in names.chunks(crate::vfs::READ_DIR_BATCH) {
        app.apply_vfs_event(VfsEvent::Entries {
            side: Side::Left,
            tab: 0,
            generation,
            batch: chunk.iter().map(Entry::file).collect(),
        });
    }
    let sorted_at = app.left.active_tab().sorted_rows;
    assert!(
        sorted_at < rows,
        "the last batches were appended without a re-sort, got {sorted_at} of {rows}"
    );
    assert!(
        sorted_at >= rows / 2,
        "and the panel is never more than one doubling behind, got {sorted_at}"
    );

    app.apply_vfs_event(VfsEvent::Done {
        side: Side::Left,
        tab: 0,
        generation,
    });
    names.sort();
    assert_eq!(
        app.left
            .active_tab()
            .entries
            .iter()
            .map(|e| e.name.clone())
            .collect::<Vec<_>>(),
        names,
        "and the finished listing is in order"
    );
}

#[test]
fn a_hit_from_a_transcoded_charset_opens_the_viewer_at_its_line() {
    // "For a content match, `Enter` opens the viewer at the
    // matching line." The searcher reports positions in the stream it
    // read, and for UTF-16, windows-1252 and CP437 that stream is the
    // decoded one - a UTF-16LE hit's offset is roughly half the file
    // offset of the line it names. Seeking to it opened the file a hundred
    // lines away from the line the status bar had just reported.
    let mut app = app_with(&[]);
    app.left.active_tab_mut().path = VfsPath::local("/root");
    let _sink = show_listing(
        &mut app,
        Side::Left,
        VirtualKind::Search,
        "[search: * \"NEEDLE\" in /root]",
    );
    let find = crate::viewer::find::FindQuery {
        input: "NEEDLE".to_string(),
        kind: crate::viewer::find::FindKind::Text,
        case: crate::config::QuickSearchCase::Sensitive,
    };
    if let Some(view) = app.left.active_tab_mut().virtual_view.as_mut() {
        view.find = Some(find);
    }
    let home = VfsPath::local("/root/notes.txt");
    let mut row = Entry::file("notes.txt");
    row.location = Some(home.clone());
    row.hit = Some(Box::new(crate::vfs::ContentHit {
        offset: 3_400,
        decoded: true,
        line: Some(201),
        line_text: "NEEDLE here".to_string(),
        charset: "UTF-16",
    }));
    app.left.active_tab_mut().entries = vec![row];

    app.open_under_cursor();
    match app.take_pending_view() {
        Some(ViewRequest::File { at, .. }) => {
            let at = at.expect("opened at the hit");
            assert_eq!(
                at.start,
                crate::viewer::HitStart::Line(201),
                "by line, because the offset counts decoded bytes"
            );
        }
        other => panic!("expected a viewer at the hit, got {other:?}"),
    }
    assert_eq!(app.message.as_deref(), Some("notes.txt: line 201"));
}

#[test]
fn a_navigation_still_clears_the_marks() {
    // The other half of cleared on directory change.
    let mut app = App::headless(Config::default(), Keymap::builtin(), Theme::blue());
    app.navigate(Side::Left, VfsPath::local("/x"));
    deliver(&mut app, Side::Left, &["a", "b"]);
    app.left.active_tab_mut().mark_all();
    assert_eq!(app.left.active_tab().marks.len(), 2);
    app.navigate(Side::Left, VfsPath::local("/y"));
    assert!(app.left.active_tab().marks.is_empty());
    assert_eq!(app.left.active_tab().cursor, 0);
}

#[test]
fn a_restored_cursor_survives_the_frames_before_the_listing_arrives() {
    // a tab holds its cursor position and is restored on
    // start. The event loop clamps and scrolls every tab before each draw,
    // including the frames before the first batch has arrived, so a
    // clamp against an empty listing would zero the restored value before
    // it could ever take effect.
    let mut app = App::headless(Config::default(), Keymap::builtin(), Theme::blue());
    app.left.active_tab_mut().cursor = 30;
    app.request_read(Side::Left, 0, VfsPath::local("/x"));
    for _ in 0..3 {
        let tab = app.left.active_tab_mut();
        tab.clamp_cursor();
        tab.scroll_into_view(20);
    }
    assert_eq!(app.left.active_tab().cursor, 30);

    let names: Vec<String> = (0..40).map(|i| format!("f{i:02}")).collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    deliver(&mut app, Side::Left, &refs);
    assert_eq!(app.left.active_tab().cursor, 30);
    assert_eq!(
        app.left.active_tab().current().map(|e| e.name.as_str()),
        Some("f30")
    );
}

#[test]
fn hidden_entries_are_filtered_when_show_hidden_is_off() {
    let mut config = Config::default();
    config.panel.show_hidden = false;
    let mut app = App::headless(config, Keymap::builtin(), Theme::blue());
    app.navigate(Side::Left, VfsPath::local("/x"));
    let generation = app.left.active_tab().generation;
    app.apply_vfs_event(VfsEvent::Entries {
        side: Side::Left,
        tab: 0,
        generation,
        batch: vec![
            // The backend sends the `..` row itself; `apply_vfs_event` no
            // longer synthesises one, so the test supplies it the way
            // `LocalFs` does.
            Entry::parent_entry(),
            Entry::file(".hidden"),
            Entry::file("visible"),
        ],
    });
    let names: Vec<&str> = app
        .left
        .active_tab()
        .entries
        .iter()
        .map(|e| e.name.as_str())
        .collect();
    assert_eq!(names, ["..", "visible"]);
}

// ------------------------------------------- archives ------

/// A throwaway directory, removed on drop. `tempfile` is not on the
/// the design dependency table, so this is built by hand - the same
/// shape `vfs::archive::tests` uses.
struct ArchiveTree {
    root: std::path::PathBuf,
}

impl ArchiveTree {
    fn new(tag: &str) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let root = std::env::temp_dir().join(format!(
            "hcmd-app-arch-{tag}-{}-{nanos:x}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).expect("create the temp tree");
        Self { root }
    }

    fn path(&self, name: &str) -> std::path::PathBuf {
        self.root.join(name)
    }
}

impl Drop for ArchiveTree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn write_zip(path: &std::path::Path, members: &[(&str, &[u8])]) {
    use std::io::Write as _;
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

/// An app whose left panel is showing `dir`, with `names` as its rows.
fn app_at(dir: &std::path::Path, names: &[&str]) -> App {
    let mut app = App::headless(Config::default(), Keymap::builtin(), Theme::blue());
    let tab = app.left.active_tab_mut();
    tab.path = VfsPath::local(dir);
    tab.entries = names.iter().map(|n| Entry::file(*n)).collect();
    let _ = app.take_pending_reads();
    app
}

/// Drive one whole read of the active tab through the real backend, the
/// way the event loop does (`spawn_read` in `main.rs`).
async fn service_reads(app: &mut App) {
    for request in app.take_pending_reads() {
        let vfs = Arc::clone(&app.vfs);
        let ReadRequest {
            side,
            tab,
            generation,
            path,
        } = request;
        let mut rx = vfs.read_dir(&path);
        let mut batch = Vec::new();
        let mut failure = None;
        while let Some(item) = rx.recv().await {
            match item {
                Ok(entry) => batch.push(entry),
                Err(err) => {
                    failure = Some(err.to_string());
                    break;
                }
            }
        }
        if !batch.is_empty() {
            app.apply_vfs_event(VfsEvent::Entries {
                side,
                tab,
                generation,
                batch,
            });
        }
        match failure {
            Some(message) => app.apply_vfs_event(VfsEvent::Failed {
                side,
                tab,
                generation,
                message,
            }),
            None => app.apply_vfs_event(VfsEvent::Done {
                side,
                tab,
                generation,
            }),
        }
    }
}

#[test]
fn enter_on_an_archive_pushes_an_archive_segment_and_the_path_shows_the_hash() {
    // "`Enter` on `foo.zip` enters it; the panel path shows
    // `…/foo.zip#/`."
    let mut app = app_at(std::path::Path::new("/a"), &["foo.zip"]);
    app.open_under_cursor();
    let tab = app.left.active_tab();
    assert_eq!(tab.path.to_string(), "/a/foo.zip#/");
    assert_eq!(tab.path.backend(), BackendKind::Archive);
    assert!(tab.loading, "it reads like any other listing");
    assert_eq!(tab.title, "foo.zip", "not the whole `#/` path");
    let reads = app.take_pending_reads();
    assert_eq!(reads.len(), 1);
    assert_eq!(reads[0].path.to_string(), "/a/foo.zip#/");
}

#[test]
fn enter_recognises_every_format_in_the_table_and_nothing_else() {
    // the eight rows, by name. Content decides in the end, but
    // the name is what makes `Enter` try.
    for name in [
        "a.zip",
        "a.tar",
        "a.tar.gz",
        "a.tgz",
        "a.tar.bz2",
        "a.tar.xz",
        "a.tar.zst",
        "a.7z",
        "a.rar",
    ] {
        let mut app = app_at(std::path::Path::new("/a"), &[name]);
        app.open_under_cursor();
        assert_eq!(
            app.left.active_tab().path.backend(),
            BackendKind::Archive,
            "{name} is in the table"
        );
    }
    let mut app = app_at(std::path::Path::new("/a"), &["notes.txt"]);
    app.open_under_cursor();
    assert_eq!(app.left.active_tab().path.backend(), BackendKind::Local);
    assert!(
        app.handoff.open.is_some(),
        "an ordinary file is still the association chain"
    );
}

#[test]
fn ctrl_pgdn_forces_entry_for_an_odd_extension_and_enter_does_not() {
    // "`Ctrl+PgDn` remains \"enter as a directory\", forcing
    // archive entry for archives with odd extensions."
    let mut app = app_at(std::path::Path::new("/a"), &["backup.dat"]);
    app.open_under_cursor();
    assert_eq!(
        app.left.active_tab().path.backend(),
        BackendKind::Local,
        "`Enter` goes by the name, and this name says nothing"
    );

    let mut app = app_at(std::path::Path::new("/a"), &["backup.dat"]);
    app.enter_as_dir();
    assert_eq!(app.left.active_tab().path.to_string(), "/a/backup.dat#/");
}

#[test]
fn enter_recognises_a_disk_image_by_name_and_lets_an_archive_name_win() {
    // the two extensions, on the same key as the design's
    // nine. The name only decides which segment is worth pushing; the
    // content decides whether it was one, a frame later.
    for name in ["ubuntu.iso", "card.img", "CARD.IMG", "backup.Iso"] {
        let mut app = app_at(std::path::Path::new("/a"), &[name]);
        app.open_under_cursor();
        assert_eq!(
            app.left.active_tab().path.backend(),
            BackendKind::Image,
            "{name} is a disk image by name"
        );
    }
    // The two claims are disjoint today: nothing in the table
    // ends in `.iso` or `.img`. The archive question is asked first
    // anyway, so a format that ever overlaps is entered as the specific
    // claim rather than as the vague one.
    for name in ["a.zip", "a.tar.gz", "a.7z", "a.rar"] {
        assert!(FormatId::from_name(name).is_some());
        assert_eq!(container_kind(name), Some(BackendKind::Archive), "{name}");
    }
    for name in ["a.iso", "a.img"] {
        assert!(
            FormatId::from_name(name).is_none(),
            "{name} makes one claim, not two"
        );
        assert_eq!(container_kind(name), Some(BackendKind::Image), "{name}");
    }
    assert_eq!(
        container_kind("disk.img.gz"),
        None,
        "a bare `.gz` is not in the table and `.gz` is not an image"
    );
    // And a name that claims neither is still the chain.
    let mut app = app_at(std::path::Path::new("/a"), &["image.png"]);
    app.open_under_cursor();
    assert_eq!(app.left.active_tab().path.backend(), BackendKind::Local);
}

#[tokio::test]
async fn ctrl_pgdn_tries_an_image_after_an_archive_and_then_gives_up() {
    // the forcing key against the "an extension
    // is a hint and never the answer": the retry is what makes that true
    // at the point of entry. One retry, and the panel goes back after it.
    let tree = ArchiveTree::new("retry");
    std::fs::write(tree.path("backup.dat"), b"neither an archive nor an image").expect("write");
    let mut app = app_at(&tree.root, &["backup.dat"]);
    app.enter_as_dir();
    assert_eq!(
        app.left.active_tab().path.backend(),
        BackendKind::Archive,
        "an archive is tried first, exactly as it always was"
    );

    service_reads(&mut app).await;
    assert_eq!(
        app.left.active_tab().path.backend(),
        BackendKind::Image,
        "the archive would not open, so the same file is tried as an image"
    );
    assert_eq!(
        app.left.active_tab().path.to_string(),
        format!("{}#/", tree.path("backup.dat").display()),
        "the retry addresses the same file, rooted at `/`"
    );

    service_reads(&mut app).await;
    let tab = app.left.active_tab();
    assert_eq!(
        tab.path,
        VfsPath::local(&tree.root),
        "and after the second failure the panel is back where it was"
    );
    assert_eq!(tab.pending_select.as_deref(), Some("backup.dat"));
    let message = app.message.clone().expect("a reason was reported");
    assert!(message.starts_with("backup.dat: "), "named it: {message}");
}

#[tokio::test]
async fn ctrl_pgdn_opens_a_disk_image_whose_extension_says_nothing() {
    // The retry earning its place: a FAT volume in a file called
    // `backup.dat` lists, on a key that used to try archives only.
    let tree = ArchiveTree::new("odd-image");
    let mut disk = std::io::Cursor::new(vec![0u8; 2 * 1024 * 1024]);
    fatfs::format_volume(&mut disk, fatfs::FormatVolumeOptions::new()).expect("format");
    disk.set_position(0);
    {
        let fs = fatfs::FileSystem::new(&mut disk, fatfs::FsOptions::new()).expect("open");
        let mut file = fs.root_dir().create_file("readme.txt").expect("create");
        std::io::Write::write_all(&mut file, b"inside a disk image").expect("write");
        std::io::Write::flush(&mut file).expect("flush");
    }
    std::fs::write(tree.path("backup.dat"), disk.into_inner()).expect("write");

    let mut app = app_at(&tree.root, &["backup.dat"]);
    app.enter_as_dir();
    service_reads(&mut app).await;
    assert_eq!(app.left.active_tab().path.backend(), BackendKind::Image);
    service_reads(&mut app).await;

    let tab = app.left.active_tab();
    assert_eq!(
        tab.path.to_string(),
        format!("{}#/", tree.path("backup.dat").display())
    );
    let names: Vec<&str> = tab.entries.iter().map(|e| e.name.as_str()).collect();
    assert!(
        names.contains(&"readme.txt"),
        "the volume listed its root: {names:?}"
    );
}

#[tokio::test]
async fn a_member_of_a_disk_image_reads_through_the_same_vfs_the_viewer_uses() {
    // the whole point, on the newest backend: `F3` and `F5` are
    // [`Vfs::open_read`], and neither has a line of image-specific code.
    let tree = ArchiveTree::new("image-member");
    let mut disk = std::io::Cursor::new(vec![0u8; 2 * 1024 * 1024]);
    fatfs::format_volume(&mut disk, fatfs::FormatVolumeOptions::new()).expect("format");
    disk.set_position(0);
    {
        let fs = fatfs::FileSystem::new(&mut disk, fatfs::FsOptions::new()).expect("open");
        let mut file = fs.root_dir().create_file("notes.txt").expect("create");
        std::io::Write::write_all(&mut file, b"the whole point").expect("write");
        std::io::Write::flush(&mut file).expect("flush");
    }
    std::fs::write(tree.path("card.img"), disk.into_inner()).expect("write");

    let mut app = app_at(&tree.root, &["card.img"]);
    app.open_under_cursor();
    service_reads(&mut app).await;
    let member = app.left.active_tab().path.join("notes.txt");

    let vfs = Arc::clone(&app.vfs);
    let body = tokio::task::spawn_blocking(move || {
        let mut reader = vfs.open_read(&member)?;
        let mut body = String::new();
        std::io::Read::read_to_string(&mut reader, &mut body)?;
        Ok::<_, crate::error::Error>(body)
    })
    .await
    .expect("join")
    .expect("read the member");
    assert_eq!(body, "the whole point");
}

#[test]
fn enter_on_click_false_leaves_archives_to_ctrl_pgdn() {
    // `[archive] enter_on_click`: "`Enter` on an archive
    // browses it."
    let mut app = app_at(std::path::Path::new("/a"), &["foo.zip"]);
    app.config.archive.enter_on_click = false;
    app.open_under_cursor();
    assert_eq!(app.left.active_tab().path.backend(), BackendKind::Local);
    app.enter_as_dir();
    assert_eq!(
        app.left.active_tab().path.to_string(),
        "/a/foo.zip#/",
        "the forcing key is not the one the setting is about"
    );
}

#[test]
fn leaving_an_archive_lands_the_cursor_back_on_it() {
    // and v0.1's `Tab::pending_select` - not a second
    // mechanism.
    let mut app = app_at(std::path::Path::new("/a"), &["foo.zip"]);
    app.open_under_cursor();
    let _ = app.take_pending_reads();

    // `..` inside the archive root.
    let tab = app.left.active_tab_mut();
    tab.entries = vec![Entry::parent_entry()];
    tab.cursor = 0;
    tab.loading = false;
    app.open_under_cursor();
    let tab = app.left.active_tab();
    assert_eq!(tab.path, VfsPath::local("/a"), "back out of the archive");
    assert_eq!(
        tab.pending_select.as_deref(),
        Some("foo.zip"),
        "the cursor lands on the archive that was being browsed"
    );
}

#[test]
fn leaving_a_directory_inside_an_archive_stays_inside_it() {
    let mut app = app_at(std::path::Path::new("/a"), &["foo.zip"]);
    app.open_under_cursor();
    let _ = app.take_pending_reads();
    let tab = app.left.active_tab_mut();
    tab.path = VfsPath::local("/a/foo.zip").with_segment(BackendKind::Archive, "/src");
    tab.entries = vec![Entry::parent_entry()];
    tab.cursor = 0;
    app.open_under_cursor();
    let tab = app.left.active_tab();
    assert_eq!(tab.path.to_string(), "/a/foo.zip#/");
    assert_eq!(tab.pending_select.as_deref(), Some("src"));
}

#[test]
fn a_nested_archive_pushes_a_second_segment() {
    // "Nested archives work through the `VfsPath` segment
    // stack."
    let mut app = app_at(std::path::Path::new("/a"), &["outer.zip"]);
    app.open_under_cursor();
    let _ = app.take_pending_reads();
    let tab = app.left.active_tab_mut();
    tab.entries = vec![Entry::parent_entry(), Entry::file("inner.tar.gz")];
    tab.cursor = 1;
    app.open_under_cursor();
    let path = &app.left.active_tab().path;
    assert_eq!(path.to_string(), "/a/outer.zip#/inner.tar.gz#/");
    assert_eq!(path.segments().len(), 3);
}

#[tokio::test]
async fn a_zip_lists_as_a_directory_through_the_panel() {
    let tree = ArchiveTree::new("list");
    let zip = tree.path("bundle.zip");
    write_zip(
        &zip,
        &[("readme.txt", b"hello"), ("src/main.rs", b"fn main() {}")],
    );
    let mut app = app_at(&tree.root, &["bundle.zip"]);
    app.open_under_cursor();
    service_reads(&mut app).await;

    let tab = app.left.active_tab();
    assert!(!tab.loading, "the listing finished");
    assert_eq!(app.message, None, "and it did not fail");
    let names: Vec<&str> = tab.entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["..", "src", "readme.txt"], "sorted, dirs first");
    assert!(
        tab.path.to_string().ends_with("bundle.zip#/"),
        "the path display: {}",
        tab.path
    );
}

#[tokio::test]
async fn marks_sorting_and_quick_search_all_work_inside_an_archive() {
    let tree = ArchiveTree::new("panelops");
    let zip = tree.path("bundle.zip");
    write_zip(
        &zip,
        &[
            ("alpha.txt", b"a"),
            ("beta.txt", b"bb"),
            ("gamma.txt", b"ccc"),
        ],
    );
    let mut app = app_at(&tree.root, &["bundle.zip"]);
    app.open_under_cursor();
    service_reads(&mut app).await;

    // Quick search - the headline behaviour, over rows that came from an
    // index rather than from `readdir`.
    assert!(app.move_cursor_to_match("gam"), "type-to-navigate");
    assert_eq!(
        app.left.active_tab().current().map(|e| e.name.as_str()),
        Some("gamma.txt")
    );

    // Marks.
    app.toggle_mark_under_cursor();
    assert!(app.left.active_tab().current_is_marked());
    assert_eq!(app.left.active_tab().marks.len(), 1);

    // Sorting, by the size column, over sizes the index reported.
    app.sort_active(SortKey::Column(ColumnId::Size));
    let sizes: Vec<u64> = app
        .left
        .active_tab()
        .entries
        .iter()
        .filter(|e| !e.is_parent)
        .map(|e| e.size)
        .collect();
    assert_eq!(sizes, [1, 2, 3], "the index's sizes, sorted");
    assert!(
        app.left.active_tab().marks.contains("gamma.txt"),
        "a mark survives a re-sort"
    );

    // The clipboard holds paths, so it addresses the member exactly.
    app.clipboard_take(false);
    let held = app.clipboard.as_ref().expect("something was copied");
    assert_eq!(held.paths.len(), 1);
    assert!(
        held.paths[0].to_string().ends_with("bundle.zip#/gamma.txt"),
        "{}",
        held.paths[0]
    );
}

#[tokio::test]
async fn a_second_tab_can_be_inside_the_same_archive() {
    let tree = ArchiveTree::new("tabs");
    let zip = tree.path("bundle.zip");
    write_zip(&zip, &[("one.txt", b"1"), ("two.txt", b"2")]);
    let mut app = app_at(&tree.root, &["bundle.zip"]);
    app.open_under_cursor();
    service_reads(&mut app).await;
    let inside = app.left.active_tab().path.clone();

    assert!(app.left.open_tab(inside.clone(), 9), "a second tab");
    let index = app.left.active_index();
    app.request_read(Side::Left, index, inside.clone());
    service_reads(&mut app).await;
    let tab = app.left.active_tab();
    assert_eq!(tab.path, inside);
    let names: Vec<&str> = tab.entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["..", "one.txt", "two.txt"]);
}

#[tokio::test]
async fn a_zip_that_is_really_html_leaves_the_panel_where_it_was() {
    // "Entering something that turns out not to be an archive after all
    // fails with the reason and leaves the panel where it was."
    let tree = ArchiveTree::new("nothtml");
    let fake = tree.path("download.zip");
    std::fs::write(&fake, b"<html><body>404 Not Found</body></html>").expect("write");
    let mut app = app_at(&tree.root, &["download.zip"]);
    app.left.active_tab_mut().cursor = 0;
    app.open_under_cursor();
    assert_eq!(app.left.active_tab().path.backend(), BackendKind::Archive);
    service_reads(&mut app).await;

    let tab = app.left.active_tab();
    assert_eq!(
        tab.path,
        VfsPath::local(&tree.root),
        "the panel went back where it was"
    );
    assert_eq!(
        tab.pending_select.as_deref(),
        Some("download.zip"),
        "and the cursor is on the file that would not open"
    );
    let message = app.message.clone().expect("a reason was reported");
    assert!(
        message.starts_with("download.zip: "),
        "named the file: {message}"
    );
    assert!(
        message.len() > "download.zip: ".len(),
        "with the backend's own reason after it: {message}"
    );
}

#[tokio::test]
async fn a_member_reads_through_the_same_vfs_the_viewer_uses() {
    // the trait is what makes the viewer, `ops` and the
    // clipboard work inside an archive with no archive-specific code.
    let tree = ArchiveTree::new("view");
    let zip = tree.path("bundle.zip");
    write_zip(&zip, &[("notes.txt", b"the whole point")]);
    let mut app = app_at(&tree.root, &["bundle.zip"]);
    app.open_under_cursor();
    service_reads(&mut app).await;
    let member = app.left.active_tab().path.join("notes.txt");

    let vfs = Arc::clone(&app.vfs);
    let body = tokio::task::spawn_blocking(move || {
        let mut reader = vfs.open_read(&member)?;
        let mut body = String::new();
        std::io::Read::read_to_string(&mut reader, &mut body)?;
        Ok::<_, crate::error::Error>(body)
    })
    .await
    .expect("join")
    .expect("read the member");
    assert_eq!(body, "the whole point");
}

#[test]
fn alt_f6_queues_a_copy_of_the_archive_root_into_the_other_panel() {
    // "`Alt+F6` unpacks the archive under the cursor to the
    // other panel's directory."
    let mut app = app_at(std::path::Path::new("/a"), &["bundle.zip"]);
    app.right.active_tab_mut().path = VfsPath::local("/dest");
    app.unpack_under_cursor();
    let jobs = app.take_pending_jobs();
    assert_eq!(jobs.len(), 1, "one job, through the ordinary copy engine");
    let spec = &jobs[0].spec;
    assert_eq!(spec.kind, JobKind::Copy);
    assert_eq!(spec.sources.len(), 1);
    assert_eq!(spec.sources[0].to_string(), "/a/bundle.zip#/");
    assert_eq!(
        spec.sources[0].backend(),
        BackendKind::Archive,
        "the source is the archive's root, not the container file"
    );
    assert_eq!(
        spec.dest.as_ref().map(ToString::to_string).as_deref(),
        Some("/dest")
    );
}

#[test]
fn alt_f6_refuses_a_directory_rather_than_starting_a_job() {
    let mut app = app_at(std::path::Path::new("/a"), &[]);
    app.left.active_tab_mut().entries = vec![Entry::dir("src")];
    app.unpack_under_cursor();
    assert!(app.take_pending_jobs().is_empty());
    assert!(
        app.message.is_some_and(|m| m.contains("not an archive")),
        "a refusal with a reason, never a silent no-op"
    );
}

#[test]
fn the_archive_session_is_not_created_until_an_archive_is_touched() {
    // `App::headless` is documented to build an application from
    // compiled-in defaults; a `0700` temp directory nobody asked for is
    // filesystem access it should not be doing.
    let app = App::headless(Config::default(), Keymap::builtin(), Theme::blue());
    assert!(app.open_archive_session().is_none());
}

#[test]
fn vfs_for_routes_by_the_innermost_segment() {
    let app = App::headless(Config::default(), Keymap::builtin(), Theme::blue());
    let local = app.vfs_for(&VfsPath::local("/etc")).expect("local");
    assert_eq!(local.kind(), BackendKind::Local);
    assert!(
        app.open_archive_session().is_none(),
        "and asked nothing of the session"
    );
}

#[tokio::test]
async fn vfs_for_an_archive_path_reports_the_formats_own_capabilities() {
    // "`Capabilities` is what the UI consults before offering
    // an operation." the design makes `.rar` read-only, and the honest
    // answer needs the open archive rather than the path alone.
    let tree = ArchiveTree::new("caps");
    let zip = tree.path("bundle.zip");
    write_zip(&zip, &[("one.txt", b"1")]);
    let app = app_at(&tree.root, &["bundle.zip"]);
    let inside = VfsPath::local(&zip).with_segment(BackendKind::Archive, "/");
    let backend = tokio::task::spawn_blocking(move || app.vfs_for(&inside))
        .await
        .expect("join")
        .expect("open the archive");
    assert_eq!(backend.kind(), BackendKind::Archive);
    assert!(backend.capabilities().writable, "a zip is writable");
    assert!(
        !backend.capabilities().can_execute,
        "nothing in an archive has a path to exec"
    );
}

#[tokio::test]
async fn two_panels_in_the_same_archive_share_one_index() {
    // "Two panels opening the same inner archive get the same
    // `Arc<ArchiveFs>`, the same index and the same temp file."
    let tree = ArchiveTree::new("share");
    let zip = tree.path("bundle.zip");
    write_zip(&zip, &[("one.txt", b"1")]);
    let mut app = app_at(&tree.root, &["bundle.zip"]);
    app.right.active_tab_mut().path = VfsPath::local(&tree.root);
    app.open_under_cursor();
    service_reads(&mut app).await;
    let inside = app.left.active_tab().path.clone();
    app.navigate(Side::Right, inside);
    service_reads(&mut app).await;
    assert_eq!(
        app.right.active_tab().entries.len(),
        app.left.active_tab().entries.len()
    );
    let session = app.open_archive_session().expect("a session was created");
    assert_eq!(session.open_count(), 1, "one archive, opened once");
}

// ------------------------------------------ the clipboard ---
