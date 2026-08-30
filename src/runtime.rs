//! The asynchronous half of the application: the event loop, and every task it
//! spawns.
//!
//! This is the layer between `dispatch`, which may touch neither the
//! filesystem nor the terminal, and the work a keystroke asks for. It owns the
//! channels - reads, jobs, console output, the viewer's line index, a connect
//! attempt's questions - the tasks on the other end of them, and the
//! generation checks that decide which of their answers still count.
//!
//! It lives in the library rather than in `src/main.rs` because that is what
//! makes it reachable: cancellation, channel ordering and the `select!` are
//! the parts most likely to be wrong, and a test cannot drive what only the
//! binary can see. `tests/runtime_loop.rs` drives the step functions below
//! with channels of its own. The binary keeps argument parsing, terminal
//! setup and teardown, and calls [`event_loop`].

use std::sync::Arc;
use std::time::Duration;

use crossterm::event::Event;
use ratatui::layout::Rect;
use tokio::sync::mpsc;

use crate::app::drives::{DrivesEvent, probe_drives};
use crate::app::reads::{CapsEvent, probe_capabilities};
use crate::app::update::UpdateEvent;
use crate::app::{
    App, ConnectRequest, InputRoute, RemoteEvent, StartedSearch, VfsEvent, ViewRequest, stream_read,
};
use crate::config::RemoteConfig;
use crate::config::{self, ColorDepth};
use crate::console::{self, Console, ConsoleEvent};
use crate::error::Result;
use crate::input::{Focus, dispatch};
use crate::ops::{self, JobUpdate};
use crate::panel::{self, Side};
use crate::remote::ftp::{BlockingHooks, FtpFs};
use crate::remote::sftp::{ConnectHooks, SftpFs};
use crate::remote::transport::RemoteTransport;
use crate::remote::{Protocol, RemoteFs, RemoteRegistry, Target, keyring, known_hosts};
use crate::search::walk::Tally;
use crate::term::{self, Term};
use crate::vfs::VfsPath;
use crate::viewer::{self, ScanJob, Viewer};
use crate::{BIN_NAME, ui};

/// How long the event loop waits with nothing happening before releasing a
/// half-spelled `[terminal.sequences]` escape sequence.
/// How many further console events are applied before the frame is drawn.
///
/// A prompt redraw arrives as several PTY reads and they belong to the same
/// instant; drawing between them shows states that live a frame. Bounded so a
/// program that never stops printing cannot starve the keyboard.
const CONSOLE_DRAIN_LIMIT: usize = 64;

/// How many further [`VfsEvent`]s are applied before the frame is drawn.
///
/// A directory arrives in `READ_DIR_BATCH`-sized batches with no throttle on
/// them, so a million-entry directory is some 7,800 events; one frame each is
/// 7,800 full-terminal redraws of a listing nobody can read that fast.
/// Everything already in the channel arrived before this frame and belongs to
/// it, exactly as the console output below does. Bounded for the same reason:
/// a walk that keeps producing must not hold the loop and starve the keyboard.
const VFS_DRAIN_LIMIT: usize = 64;

/// How many further input events already waiting are applied before drawing.
///
/// See the drain in the event loop. Large enough that a held key at any sane
/// autorepeat rate never backs up, small enough that a runaway source cannot
/// hold the loop.
const KEY_DRAIN_LIMIT: usize = 256;

/// How deep the [`RemoteEvent`] channel is.
///
/// A connect attempt has at most one question outstanding at a time and there
/// are at most [`crate::remote::MAX_CONNECTIONS`] of them, so this is
/// never full in practice; it is bounded because every channel here is.
const REMOTE_CHANNEL_DEPTH: usize = 32;

/// How many keystrokes are held while a viewer is being opened.
///
/// One gesture is a handful of keys. A held-down key is not a gesture, and
/// replaying five hundred of them into a viewer that has just appeared is not
/// what the person leaning on the key meant.
const HELD_KEY_LIMIT: usize = 64;

const SEQUENCE_TIMEOUT: Duration = Duration::from_millis(250);

/// How deep the [`DrivesEvent`] channel is.
///
/// One probe is outstanding at a time, so this is never full; it is bounded
/// because every channel here is.
const DRIVES_CHANNEL_DEPTH: usize = 4;

/// How deep the [`CapsEvent`] channel is.
///
/// One per completed listing, and there are at most nine tabs a panel, so a
/// restored session's worth of reads finishing together still fits.
const CAPS_CHANNEL_DEPTH: usize = 32;

/// How deep the [`ConfigWritten`] channel is.
///
/// Two files, one write of each outstanding at a time.
const CONFIG_CHANNEL_DEPTH: usize = 4;

/// A configuration file the blocking pool has finished writing, and what it
/// could not do.
///
/// The success case carries `None` and is still sent, because it is what says
/// the file is free to be written again - see [`ConfigWrites`].
pub enum ConfigWritten {
    /// `hosts.toml`.
    Hosts(Option<String>),
    /// `hotlist.toml`.
    Hotlist(Option<String>),
}

/// Which configuration files have a write in flight.
///
/// One at a time each, so two writes of one file cannot land out of order:
/// both rewrite the whole file, and the older one landing last would undo the
/// newer. The dirty flag stays set in the meantime, so nothing is lost - the
/// next frame after the answer queues it.
#[derive(Debug, Default)]
pub struct ConfigWrites {
    /// `hosts.toml`.
    hosts: bool,
    /// `hotlist.toml`.
    hotlist: bool,
}

/// Queue whatever the host book and the hotlist still owe their files.
///
/// Both writes are `create_dir_all` plus `std::fs::write` on the config
/// directory, which is I/O, and the loop that calls this is the thread that
/// draws - so neither happens here. A file with a write already in flight is
/// left for a later frame.
pub fn service_config_writes(
    app: &mut App,
    tx: &mpsc::Sender<ConfigWritten>,
    writing: &mut ConfigWrites,
) {
    if !writing.hosts
        && let Some(hosts) = app.take_hosts_write()
    {
        writing.hosts = true;
        let tx = tx.clone();
        tokio::task::spawn_blocking(move || {
            let problem = crate::remote::hosts::store(&hosts)
                .err()
                .map(|err| err.to_string());
            let _ = tx.blocking_send(ConfigWritten::Hosts(problem));
        });
    }
    if !writing.hotlist
        && let Some(entries) = app.take_hotlist_write()
    {
        writing.hotlist = true;
        let tx = tx.clone();
        tokio::task::spawn_blocking(move || {
            let problem = crate::devices::hotlist::store(&entries)
                .err()
                .map(|err| err.to_string());
            let _ = tx.blocking_send(ConfigWritten::Hotlist(problem));
        });
    }
}

/// Apply one directory-read event and start the capability probe it asks for.
///
/// [`App::apply_vfs_event`] hands back the one question a completed listing
/// cannot answer where it stands: [`crate::vfs::Vfs::capabilities_for`]
/// blocks, and this loop draws.
pub fn apply_vfs(app: &mut App, event: VfsEvent, caps_tx: &mpsc::Sender<CapsEvent>) {
    let Some(request) = app.apply_read_event(event) else {
        return;
    };
    tokio::spawn(probe_capabilities(
        Arc::clone(&app.vfs),
        request,
        caps_tx.clone(),
    ));
}

/// Set the terminal up, restore the session and run until something asks to
/// stop.
///
/// Everything the application does that is not a keystroke happens here: the
/// channels are created, the tasks that answer them are spawned, and one pass
/// round the loop services what the last keystroke queued, draws a frame, and
/// waits for whichever of them speaks first. It returns when the user quits,
/// when `SIGTERM` or `SIGHUP` arrives, or when the input thread goes away -
/// and saves the tabs on the way out.
pub async fn event_loop() -> Result<()> {
    // `config::load` has already resolved `ui.ascii_borders` against the
    // locale; it does the same on a `Ctrl+Alt+R` reload.
    let loaded = config::load();
    let mut app = App::new(loaded, App::default_start());
    // Detected from the environment, with `[terminal] colors` overriding.
    //
    app.color_depth = ColorDepth::resolve(app.config.terminal.colors);

    // Signals first, before raw mode exists to leak. `Term::init` enables raw
    // mode as its very first action and can then sit in the keyboard capability
    // query for up to half a second on a terminal that never answers; a
    // `kill -TERM` in that window would otherwise hit the default disposition
    // and kill the process with raw mode on and the alternate screen up.
    // tokio installs each handler when its stream is created, and
    // a signal that arrives before the first `recv` is remembered, so arming
    // both here closes the window without changing the behaviour.
    //
    // SIGINT restores and exits. SIGTERM *and SIGHUP* break the loop below and
    // restore on the way out of `run`, which is the graceful path - and the
    // only one that reaches `panel::state::save`, so closing the terminal
    // window keeps the session's tabs exactly as `kill -TERM` does.
    // The guard still ends the process on SIGHUP after
    // `Term::HANGUP_GRACE`, for a loop that never gets there.
    Term::spawn_signal_guard(false)?;
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut sighup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;

    let mut term = Term::init(&app.config.terminal)?;
    // Raw mode and the alternate screen are on as of the line above, which is
    // exactly where the panic rule has to hold. Inert unless
    // `HCMD_PANIC_TEST` is set.
    term::panic_test_hook(term::PANIC_STAGE_START);
    app.keyboard.enhanced = term.enhanced_keyboard();
    app.warnings.extend(term.warnings().iter().cloned());
    if app.config.ui.mouse {
        term.set_mouse(true)?;
    }

    // the Load/Save tab and its three combo boxes, read once here
    // because `Alt+F7` opens the dialog from `dispatch`, which may not touch
    // the filesystem. Before the warning count, so a
    // `searches.toml` that will not parse is counted with the rest.
    app.load_search_state();
    // the host book, read once here for the same reason
    // `searches.toml` is: `Ctrl+F` opens the dialog from `dispatch`, which may
    // not touch the filesystem.
    app.load_hosts();
    // the hotlist, read once here for the same reason: `Ctrl+D`
    // builds its popup from what is already in memory.
    app.load_hotlist();

    if !app.warnings.is_empty() {
        app.message = Some(format!(
            "{} configuration warning(s); run `{BIN_NAME} --check-config`",
            app.warnings.len()
        ));
    }

    // tabs are persisted per panel and restored on start. The
    // saved paths win over the working directory - that is what the state file
    // is for - and a missing or unreadable file simply leaves both panels on
    // the single tab `App::new` built at the cwd.
    let (saved, state_warnings) = panel::state::load();
    app.warnings.extend(state_warnings);
    let max_tabs = app.config.panel.max_tabs;
    panel::state::restore(&mut app.left, &saved.left, max_tabs);
    panel::state::restore(&mut app.right, &saved.right, max_tabs);

    // Every tab reads, not just the active one, so switching to a restored tab
    // shows its listing instead of an empty panel. Reads are streamed and
    // cancellable, and there are at most nine per panel.
    for side in [Side::Left, Side::Right] {
        let paths: Vec<VfsPath> = app
            .panel(side)
            .tabs()
            .iter()
            .map(|tab| tab.path.clone())
            .collect();
        for (index, path) in paths.into_iter().enumerate() {
            app.request_read(side, index, path);
        }
    }

    // the shell is started **at application start**, not on the
    // first `Ctrl+O`, so its scrollback survives toggling. After the tab state
    // has been restored, so it opens in the directory the panel is actually
    // showing rather than in the process's working directory.
    let (console_tx, mut console_rx) = mpsc::channel::<ConsoleEvent>(console::OUTPUT_CHANNEL_DEPTH);
    start_console(&mut app, &mut term, &console_tx);

    let (vfs_tx, mut vfs_rx) = mpsc::channel::<VfsEvent>(256);
    // a connect task is long-lived and has questions
    // for the user, so it talks to the event loop over its own channel and
    // gets its answers back through `oneshot` senders it puts inside the
    // events.
    let (remote_tx, mut remote_rx) = mpsc::channel::<RemoteEvent>(REMOTE_CHANNEL_DEPTH);
    // the viewer's line index is built in a background task and
    // reported in `viewer.index_chunk` steps.
    let (index_tx, mut index_rx) =
        mpsc::channel::<viewer::index::IndexBatch>(viewer::index::INDEX_CHANNEL_DEPTH);
    // "a match counter fills in behind the cursor while the scan
    // continues in the background". Same shape as the line index, same reason.
    let (find_tx, mut find_rx) =
        mpsc::channel::<viewer::find::FindBatch>(viewer::find::FIND_CHANNEL_DEPTH);
    // "a 40 GB file must open as fast as a 4 KB one". True of a
    // local file, where opening is an `open(2)` and one window; **not** true of
    // an archive member, where the first byte costs decompressing everything
    // before it - measured at 0.35 s for the hundredth member of a 210 MiB
    // `.tar.gz`, and minutes for one near the end of a large `.tar.xz`. Doing
    // that on this task would be a frame not drawn and a key, `Esc` included,
    // not read for as long as it took. So the open happens on the blocking
    // pool and the built viewer arrives here, the same shape as its own index
    // scan.
    let (view_tx, mut view_rx) = mpsc::channel::<OpenedViewer>(4);
    // While that open is in flight the keys the user types belong to the
    // viewer they asked for, not to the panel behind it: `F3` `F7` `hello` is
    // one gesture, and letting the two keys after `F3` reach the panel would
    // open a "Create directory" prompt over a viewer that is on its way. They
    // are held here and replayed into the viewer the moment it arrives, so
    // moving the open off this task changes what the application *waits* for
    // and nothing about what the keystrokes mean.
    let mut opening_viewer = false;
    let mut held_keys: Vec<Event> = Vec::new();
    let (job_tx, mut job_rx) = mpsc::channel::<JobUpdate>(ops::JOB_CHANNEL_DEPTH);
    // what the finished walk passed over, so the one line the honesty rule
    // asks for can be shown. A channel rather than polling the task handle,
    // because the walk ends on the blocking pool at a moment no frame is tied
    // to and a poll would report it late or not at all.
    let (tally_tx, mut tally_rx) = mpsc::channel::<FinishedSearch>(4);
    // The drive popup's blocking half, and the one thing on this loop that
    // must never run on it: `sysinfo` reads `/proc/mounts` and statvfs's every
    // mount, and greying a hotlist row is a `stat` that a hung NFS or sshfs
    // mount never answers. `drives_probe` is true while one is out, so a
    // deadline that keeps coming due cannot pile blocking threads behind a
    // mount that has stopped answering.
    let (drives_tx, mut drives_rx) = mpsc::channel::<DrivesEvent>(DRIVES_CHANNEL_DEPTH);
    let mut drives_probe = false;
    // What a completed listing still owes the panel. `capabilities_for` opens
    // an archive to learn its format and waits on a remote transport, so it is
    // asked here and answered off this thread; the tab shows its backend's own
    // conservative answer until this arrives.
    let (caps_tx, mut caps_rx) = mpsc::channel::<CapsEvent>(CAPS_CHANNEL_DEPTH);
    // `hosts.toml` and `hotlist.toml`, written on the blocking pool. Only the
    // failures come back, and only to the status line: the design keeps
    // configuration problems non-fatal.
    let (config_tx, mut config_rx) = mpsc::channel::<ConfigWritten>(CONFIG_CHANNEL_DEPTH);
    let mut writing = ConfigWrites::default();
    // The version check, asked on the blocking pool. One question, one
    // answer, and a failure that reaches the status line and stops there.
    let (update_tx, mut update_rx) =
        mpsc::channel::<UpdateEvent>(crate::app::update::UPDATE_CHANNEL_DEPTH);
    let (info_tx, mut info_rx) = mpsc::channel::<crate::viewer::fileinfo::FileInfo>(
        crate::app::fileinfo::FILE_INFO_CHANNEL_DEPTH,
    );
    let (key_tx, mut key_rx) = mpsc::channel::<std::io::Result<Event>>(term::EVENT_CHANNEL_DEPTH);
    term::spawn_event_thread(key_tx);

    loop {
        // `Alt+F7` and `Ctrl+B` queued a search and this starts
        // it, for the same reason a read is serviced here - `dispatch` may not
        // touch the filesystem, and starting one
        // registers a listing and spawns a walk. Immediately before the reads
        // are drained, so the listing's own read is spawned on this frame and
        // the panel fills from the very first batch.
        // what the Load/Save tab and the combo boxes changed,
        // written here because `dispatch` may not touch the filesystem.
        // A no-op on every frame but the ones after
        // a search.
        app.service_search_state();
        service_search(&mut app, &tally_tx);
        // `Start!` or `Undo` answered, and this turns the pairs
        // into a job. Here rather than in `dispatch` for the same reason the
        // search above is: queueing one touches the filesystem.
        service_rename(&mut app);
        // the `Ctrl+F`, answered. Started here rather than in
        // `dispatch` for the reason the search above is - and started rather
        // than finished, because a connect can sit waiting for a host-key or
        // password dialog while the UI keeps drawing.
        service_connect(&mut app, &remote_tx);
        // what the connect dialog changed in the host book, and
        // what `Ctrl+Shift+D` changed in `hotlist.toml`. Both are queued here
        // and written on the blocking pool: a `create_dir_all` and a
        // `std::fs::write` are I/O, and this loop is the render thread.
        service_config_writes(&mut app, &config_tx, &mut writing);
        // The version check `Alt+U` asked for. Here rather than in `dispatch`
        // for the reason the writes above are: a request over the network is
        // I/O, and this loop is the render thread.
        app.service_update_check(&update_tx);
        app.service_file_info(&info_tx);
        // `Alt+F1` / `Ctrl+D` asked for a popup. The popup goes up
        // on this frame from what is already in memory; enumerating mounts
        // reads `/proc/mounts` and greying a missing hotlist row is a `stat`
        // per entry, so both go to the blocking pool and fill the popup in
        // when they answer. One hotlist entry on a hung mount would otherwise
        // stop every frame after it.
        //
        // Not while an answer is still out: a probe that never comes back is
        // exactly the hung mount this is written around, and piling a second
        // blocking thread on it every keystroke helps nobody. The request
        // stays in its slot and is serviced when the pool is free again.
        if !drives_probe && let Some(probe) = app.service_drives() {
            drives_probe = true;
            tokio::spawn(probe_drives(probe, drives_tx.clone()));
        }
        // the live list, kept live by re-enumeration rather than
        // by a watch - the design records why no crate can
        // watch the mount table here. On the blocking pool for the reason the
        // first enumeration is: `sysinfo` statvfs's every mount.
        if let Some(probe) = app.service_drives_poll(std::time::Instant::now(), drives_probe) {
            drives_probe = true;
            tokio::spawn(probe_drives(probe, drives_tx.clone()));
        }
        // the disconnected state: the last listing stays on
        // screen, greyed, and the path is not lost.
        app.service_remote_liveness();
        for request in app.take_pending_reads() {
            let (side, tab) = (request.side, request.tab);
            let task = tokio::spawn(stream_read(Arc::clone(&app.vfs), request, vfs_tx.clone()));
            // The handle, so the *next* read of this tab can stop this walk
            // instead of leaving it to fill a channel whose every batch the
            // generation check then discards (`App::register_read`).
            app.register_read(side, tab, task.abort_handle());
        }
        // the gates, in front of the user and before anything is
        // touched: a write into a compressed tar over `rewrite_max_size` is
        // refused here, and one over `rewrite_warn_size` is asked about. Here
        // rather than in `dispatch` for the same reason the pack below is -
        // both gates need the archive's size and the free space beside it.
        app.service_rewrite_gates();
        // Jobs are spawned exactly as reads are: `dispatch` queued them and
        // never touched the filesystem itself.
        for request in app.take_pending_jobs() {
            let handle = ops::spawn(
                Arc::clone(&app.vfs),
                request.id,
                request.spec,
                job_tx.clone(),
                &app.config.ops,
            );
            app.register_job(handle);
        }
        if app.reload_requested {
            app.perform_reload()?;
        }
        // whether `F8` can keep its promise is decided before
        // the operation starts. `dispatch` may not ask the filesystem, so the
        // keystroke queued the question and this answers it and pushes the
        // confirmation - one frame later, before anything is drawn.
        app.service_trash_probe();

        // the `F4`: leave the alternate screen, run the editor with
        // inherited stdio, wait, come back, reread. Serviced here for the same
        // reason a read or a job is - `dispatch` may not own the terminal, and
        // this takes it away entirely for as long as the editor runs.
        ops::editor::service(&mut app, &mut term)?;

        // `Enter` on a file queued an open and this resolves it -
        // one read of the file's head, then the execute policy, a handler, the
        // desktop or the internal viewer. Immediately after the editor for the
        // same reason the editor is here: a handler is spawned exactly as the
        // editor is, and `execute_in = "console"` writes to the PTY.
        app.service_open(&mut term)?;

        // Tell the panels how many entry rows they got, before drawing, so
        // PgUp/PgDn and scrolling have a real number to work with - and so a
        // resize re-clamps the cursor instead of leaving it off-screen.
        // `draw` only gets `&App`, so the count has to be pushed in here; the
        // arithmetic (tab bar, path line, header, rule, status line) belongs to
        // the renderer, which is the only thing that knows what it drew.
        // `Ctrl+O` on a shell that has died (report it, and offer
        // a new one on the next `Ctrl+O`).
        if app.console.restart_requested {
            app.console.restart_requested = false;
            start_console(&mut app, &mut term, &console_tx);
            if app.console_owns_cmdline() {
                app.set_focus(Focus::Console);
            }
        }

        // What the last keystroke queued for the shell. Queued
        // rather than written by `dispatch`, because a pty write can block and
        // `dispatch` must stay drivable with no terminal at all.
        let (to_shell, origin) = app.take_pending_shell();
        if let Some(console) = app.console.shell.as_mut() {
            console.write(&to_shell, origin);
        }

        let size = term.terminal().size()?;
        // "The PTY is resized with the terminal." The shell is
        // given the *whole* screen even while the panels are showing, because
        // `Ctrl+O` hands it exactly that - a `vim` started from
        // the command line must already be the right size when the panels get
        // out of its way.
        if let Some(console) = app.console.shell.as_mut() {
            console.resize(size.height, size.width);
        }
        let area = Rect::new(0, 0, size.width, size.height);
        ui::sync_view_rows(&mut app, area);
        // And the same for the dialog stack, which is laid out at the interior
        // each frame will hand it so that a dialog that scrolls remembers where
        // it had scrolled to without needing interior mutability to say so.
        ui::sync_dialog_layout(&mut app, area);
        // And the same measurement for a viewer, whether or not one is up: a
        // viewer opened between two frames is laid out at it as it is pushed,
        // so the keys held while it opened are applied to a viewer that knows
        // how big the screen is.
        let view_body = (
            ui::viewer::body_rows(area),
            ui::viewer::body_cols(&app, area),
        );
        app.set_viewer_view(view_body.0, view_body.1);
        for panel in [&mut app.left, &mut app.right] {
            let rows = panel.view_rows;
            let tab = panel.active_tab_mut();
            tab.clamp_cursor();
            tab.scroll_into_view(rows);
        }

        // a shell can die without the PTY saying so - a
        // backgrounded job holds the slave open, so `read` never returns EOF
        // and the reader thread never reports one. The child itself is the
        // authoritative signal, and asking it is one non-blocking syscall a
        // frame.
        app.service_console_exit();
        // the `auto`: `Enter` armed a deadline instead of switching,
        // and this is where it is answered.
        app.service_console_switch(std::time::Instant::now());
        // The theme picker previews by moving its cursor, so the running
        // theme is brought into step with it once a frame, before the draw
        // that shows it.
        app.service_theme_preview();

        // The job dialogs are views of `app.jobs`, so they are
        // brought into step with it once a frame, immediately before the draw
        // that shows them: a progress dialog opens, a conflict is raised, a
        // finished job reports and the panels re-read.
        app.sync_job_dialogs();

        // the `Alt+F5`: the dialog answered, and this creates the
        // archive and queues the copy that fills it. Serviced here for the
        // same reason a read or a job is - `dispatch` may not touch the
        // filesystem, and creating a container is touching it.
        if let Some(request) = app.take_pending_pack() {
            app.perform_pack(request);
        }

        // `F3` queued a viewer and this opens it, for the same
        // reason a directory read is serviced here - `dispatch` may not touch
        // the filesystem, and opening also spawns the background index scan.
        open_pending_viewer(&mut app, &view_tx, &index_tx, &mut opening_viewer);

        // `Ctrl+C` queued a copy and this performs it, for the
        // same reason - it reads the selection, and `dispatch` may not read.
        service_viewer_copy(&mut app);

        // A keystroke in the find bar queued a counter; this spawns it, for the
        // same reason the line index is spawned here and not in `dispatch`.
        for job in app.take_find_jobs() {
            spawn_find(job, find_tx.clone());
        }

        // the quick view: the debounce expired, so this opens the
        // file the cursor has been resting on. After `ui::sync_view_rows`,
        // which measured the panel it will be drawn in, and before the draw,
        // so a viewer opened between two frames is laid out at the size it is
        // about to appear at.
        app.service_quick_view(std::time::Instant::now());
        // And its line index, spawned exactly as the full-screen viewer's is:
        // a `read` loop over a file that may be 40 GB has no business on the
        // path that draws a frame.
        if let Some(job) = app.quick_viewer_mut().and_then(viewer::Viewer::take_scan) {
            let tx = index_tx.clone();
            let ScanJob {
                id,
                source,
                chunk,
                cancel,
            } = job;
            tokio::task::spawn_blocking(move || {
                viewer::index::scan(id, source, chunk, cancel, tx);
            });
        }

        // And lay it out, before the draw. `ui::draw` only has `&App`, and
        // reading the visible window is the model's job.
        if ui::viewer::is_backdrop(&app) {
            let rows = ui::viewer::body_rows(area);
            let cols = ui::viewer::body_cols(&app, area);
            app.service_viewer(rows, cols);
            // the restore rule, asked about the viewer's state rather
            // than about start-up: raw mode, the alternate screen, an open
            // file and a running index scan. Inert unless `HCMD_PANIC_TEST`
            // names this stage.
            term::panic_test_hook(term::PANIC_STAGE_VIEWER);
        }

        // One measurement per frame, shared by the layout, the painter and the
        // caret. It needs `&mut`, so it happens here rather than
        // inside the renderer.
        app.refresh_console_block();
        term.terminal().draw(|f| ui::draw(f, &app))?;

        // How long to wait for something to happen. Ordinarily the escape
        // sequence timeout below; shorter while the `auto` is
        // waiting on its deadline, so a `sleep 30` that says nothing at all
        // still gets the screen at `switch_delay` rather than at the next
        // keystroke. Never shorter while a sequence is half-spelled: the timer
        // arm *releases* that sequence, and releasing it early would break the
        // `[terminal.sequences]` contract.
        // The console's repaint settle window is the other deadline: the docked
        // command line holds its previous rendering while the shell's output is
        // still arriving, so *something* has to wake the loop when the quiet
        // arrives or the settled prompt is drawn only at the next keystroke.
        // the debounce and the re-enumeration are two
        // more deadlines of the same kind: nothing wakes the loop when a
        // cursor has simply been still for 150 ms, or when a volume was
        // mounted while a popup is open, so the timeout has to.
        let deadline = [
            app.console_switch_deadline(),
            app.console_settle_deadline(),
            app.quick_view_deadline(),
            app.drives_deadline(),
        ]
        .into_iter()
        .flatten()
        .min();
        let idle = match deadline {
            Some(at) if !term.sequence_pending() => SEQUENCE_TIMEOUT
                .min(at.saturating_duration_since(std::time::Instant::now()))
                .max(Duration::from_millis(1)),
            _ => SEQUENCE_TIMEOUT,
        };

        tokio::select! {
            maybe = key_rx.recv() => {
                match maybe {
                    Some(Ok(event)) => {
                        let route = app.input_route();
                        apply_input(&mut app, &mut term, event, opening_viewer, &mut held_keys)?;
                        drain_input(
                            &mut app,
                            &mut term,
                            &mut key_rx,
                            route,
                            opening_viewer,
                            &mut held_keys,
                        )?;
                    }
                    Some(Err(err)) => return Err(err.into()),
                    None => break,
                }
            }
            Some(event) = vfs_rx.recv() => {
                apply_vfs(&mut app, event, &caps_tx);
                // **Drain what else is already waiting before drawing**, for
                // the reason the console output below is drained and keys are:
                // a listing arrives in 128-row batches, every batch here is a
                // full-terminal redraw, and a large directory produces
                // thousands of them. The batches already in the channel
                // arrived before this frame, so they belong to this frame -
                // and drawing only the listing they add up to is both faster
                // and what the panel is meant to show.
                //
                // Bounded, because a walk that never stops producing would
                // otherwise keep the loop here and starve the keyboard.
                for _ in 0..VFS_DRAIN_LIMIT {
                    match vfs_rx.try_recv() {
                        Ok(event) => apply_vfs(&mut app, event, &caps_tx),
                        Err(_) => break,
                    }
                }
            }
            Some(event) = drives_rx.recv() => {
                // The mount table and the hotlist `stat`s, back from the
                // blocking pool. The popup that has been on screen since the
                // key was pressed fills in here.
                drives_probe = false;
                app.apply_drives_event(event);
            }
            Some(event) = caps_rx.recv() => {
                // What the panel's directory can do, back from the blocking
                // pool. Dropped inside `apply_caps_event` when the tab has
                // moved on, the same generation check a listing gets.
                app.apply_caps_event(event);
            }
            Some(written) = config_rx.recv() => {
                // A configuration file has been written. Nothing to show
                // unless it could not be: the design keeps that non-fatal, and
                // a status line is the whole of the report.
                let problem = match written {
                    ConfigWritten::Hosts(problem) => {
                        writing.hosts = false;
                        problem
                    }
                    ConfigWritten::Hotlist(problem) => {
                        writing.hotlist = false;
                        problem
                    }
                };
                if let Some(problem) = problem {
                    app.message = Some(problem);
                }
            }
            Some(info) = info_rx.recv() => {
                // The read is done and the dialog is the answer. It is pushed
                // here rather than in the blocking task because a dialog is
                // application state and the task has none of it.
                app.push_dialog(Box::new(
                    crate::ui::dialog::fileinfo::FileInfoDialog::new(&info),
                ));
            }
            Some(event) = update_rx.recv() => {
                // What GitHub said about the latest release, or why it could
                // not be asked. Either way it is one status line.
                app.apply_update_event(event);
            }
            Some(event) = remote_rx.recv() => {
                // A question from a connect task, or its answer. An event from
                // an attempt the user has abandoned is dropped inside
                // `apply_remote_event`, which drops its reply channel with it -
                // and a dropped reply is a refusal.
                app.apply_remote_event(event);
            }
            Some(update) = job_rx.recv() => {
                app.apply_job_event(update);
            }
            Some(opened) = view_rx.recv() => {
                opening_viewer = false;
                push_opened_viewer(&mut app, opened, &index_tx);
                // The keys held while the open ran are applied here, and
                // `App::push_viewer` has just laid the viewer out at the size
                // the last frame measured - without that a `PgDn` waiting
                // behind a slow open would page by one row, because a viewer
                // that has never been laid out believes the screen is zero rows
                // high.
                for event in std::mem::take(&mut held_keys) {
                    apply_input(&mut app, &mut term, event, false, &mut held_keys)?;
                }
            }
            Some(batch) = index_rx.recv() => {
                // A batch whose viewer has been closed is dropped inside
                // `apply_index_batch`.
                app.apply_index_batch(&batch);
            }
            Some(finished) = tally_rx.recv() => {
                // one line at the end saying what the
                // walk could not read, and nothing at all when it read
                // everything.
                app.report_search_tally(&finished.started, &finished.tally);
            }
            Some(batch) = find_rx.recv() => {
                // Likewise a batch from a search two keystrokes ago: the
                // generation on it no longer matches.
                app.apply_find_batch(&batch);
            }
            Some(event) = console_rx.recv() => {
                // Output from the shell, or the news that it has gone. Either
                // way the application carries on.
                app.apply_console_event(event);
                // **Drain what else is already waiting before drawing.**
                //
                // A shell redraws its prompt as several escape sequences and
                // does not have to put them in one write, so applying one read
                // per frame draws the half-finished states in between: the
                // command line shows characters that live a frame and vanish,
                // which on a slow link is most of what the eye sees. Everything
                // already in the channel belongs to the same instant, so it is
                // applied in the same frame.
                //
                // Bounded, because a program that never stops printing would
                // otherwise keep the loop here and starve the keyboard.
                for _ in 0..CONSOLE_DRAIN_LIMIT {
                    match console_rx.try_recv() {
                        Ok(event) => app.apply_console_event(event),
                        Err(_) => break,
                    }
                }
            }
            _ = sigterm.recv() => break,
            // The terminal has gone. Break rather than die where the loop
            // stands: leaving through the bottom of this function is what
            // restores the terminal and saves the tabs.
            _ = sighup.recv() => break,
            () = tokio::time::sleep(idle) => {
                // Nothing arrived, so a half-typed escape sequence is not
                // going to be completed. Release it rather than swallow it.
                for key in term.flush_sequence() {
                    dispatch(&mut app, key)?;
                }
            }
        }

        if app.should_quit {
            break;
        }
    }

    // Every open viewer goes with the session: a `Viewer`'s `Drop` sets its
    // scan's cancel flag, and a blocking-pool thread still reading a 40 GB file
    // would otherwise keep the process alive past the last frame.
    //
    app.close_viewers();

    // Leave the alternate screen and raw mode *before* anything is printed.
    // `Drop for Term` would do it a few lines later on its own, and a message
    // written in between lands on the alternate screen buffer and is thrown
    // away by the `LeaveAlternateScreen` that follows - the user would never
    // see that their tabs had not been saved. `run` promises the same thing
    // for its own errors.
    drop(term);

    // Best effort: failing to write the state file must not turn a clean quit
    // into an error exit (the design - configuration problems are never
    // fatal, and this is less than configuration).
    if let Err(err) = panel::state::save(&app.left, &app.right) {
        eprintln!("{BIN_NAME}: could not save tab state: {err}");
    }

    Ok(())
}

/// Apply the input events already waiting, before the frame is drawn.
///
/// A held key autorepeats faster than a frame takes to draw, so one key per
/// frame falls behind and the backlog keeps playing after the key is released -
/// hold `PageDown` in the viewer for a second and it scrolls for a second more.
/// Everything already in the channel arrived before this frame, so it belongs
/// to this frame, and drawing only the state they add up to is both faster and
/// what the user meant. The console channel is drained for the same reason.
///
/// `route` is where input went before the key that led here. Draining stops the
/// moment it changes: see [`App::input_route`] for why `F3` then `2` must not
/// be drained together. It stops on anything queued for the shell too, so the
/// console keeps one pty write per keystroke. Bounded as well, because a paste arriving as keystrokes
/// on a terminal with no bracketed-paste support would otherwise keep the loop
/// here and starve everything else.
pub fn drain_input(
    app: &mut App,
    term: &mut term::Term,
    key_rx: &mut mpsc::Receiver<std::io::Result<Event>>,
    route: InputRoute,
    opening_viewer: bool,
    held: &mut Vec<Event>,
) -> Result<()> {
    let mut route = route;
    for _ in 0..KEY_DRAIN_LIMIT {
        if app.input_route() != route {
            return Ok(());
        }
        // And never across a write to the shell. A pty write is where a line
        // ends: coalescing the `cat` a user typed with the line they typed for
        // `cat` hands bash both in one read, readline takes the second as the
        // next command, and the running program never sees it (the design -
        // what reaches the shell reaches it as it was typed). One frame per
        // keystroke is what the console had before this drain existed, and it
        // is what the console keeps.
        if !app.pending_shell().is_empty() {
            return Ok(());
        }
        route = app.input_route();
        match key_rx.try_recv() {
            Ok(Ok(event)) => apply_input(app, term, event, opening_viewer, held)?,
            Ok(Err(err)) => return Err(err.into()),
            Err(_) => return Ok(()),
        }
    }
    Ok(())
}

/// Apply one terminal input event.
///
/// Split out of the event loop only so the loop can apply the events already
/// waiting in the channel through the same path as the one it just received.
pub fn apply_input(
    app: &mut App,
    term: &mut term::Term,
    event: Event,
    opening_viewer: bool,
    held: &mut Vec<Event>,
) -> Result<()> {
    // A viewer is being built on the blocking pool: these keys were typed at
    // it. See `open_pending_viewer`. Bounded, because a key held down while a
    // 4 GB `.tar.xz` is being wound through is not a queue anybody wants
    // replayed.
    if opening_viewer && matches!(event, Event::Key(_) | Event::Paste(_)) {
        if held.len() < HELD_KEY_LIMIT {
            held.push(event);
        }
        return Ok(());
    }
    match event {
        Event::Key(key) => {
            track_modifiers(app, key);
            // `dispatch` clears `app.message` per keypress; it is not cleared
            // here as well, or a key that `decode` holds back would wipe the
            // message that is still on screen explaining the last one.
            // [terminal.sequences] may rewrite the key, and may hold it back
            // while a configured sequence is still being spelled out.
            //
            for key in term.decode(key) {
                dispatch(app, key)?;
            }
        }
        Event::Paste(text) => {
            app.message = None;
            paste(app, &text);
        }
        // Resize needs no work of its own: the top of the loop re-lays out and
        // re-clamps before every draw.
        Event::FocusGained | Event::FocusLost | Event::Mouse(_) | Event::Resize(_, _) => {}
    }
    Ok(())
}

/// A walk that has ended, with what it passed over.
pub struct FinishedSearch {
    /// The search these results belong to.
    pub started: StartedSearch,
    /// What the walk could not read.
    pub tally: Tally,
}

/// Start the search `Alt+F7`, `Alt+Shift+F7` or `Ctrl+B` queued.
///
/// Here rather than in `dispatch` for the same reason `open_pending_viewer` is
/// here: it registers a listing, re-points a tab and spawns a walk.
/// One search per frame, because one is all a
/// keystroke can ask for.
///
/// The walk's own task is watched by a second, tiny task whose only job is to
/// carry the [`search::walk::Tally`] back into the loop when the walk ends:
/// the design does not let a search that could not read three directories
/// pass over them in silence.
pub fn service_search(app: &mut App, tally_tx: &mpsc::Sender<FinishedSearch>) {
    let Some(request) = app.take_pending_search() else {
        return;
    };
    let Some(mut started) = app.start_search(*request) else {
        return;
    };
    let tx = tally_tx.clone();
    tokio::spawn(async move {
        // A walk that panicked has already poisoned nothing: the listing is
        // closed by the sink's own `Drop`, and there is no tally to report.
        let Ok(tally) = (&mut started.walk).await else {
            return;
        };
        let _ = tx.send(FinishedSearch { started, tally }).await;
    });
}

/// Start the connection `Ctrl+F` asked for.
///
/// Here rather than in `dispatch` because connecting is I/O,
/// and **started** rather than finished for the
/// reason the design gives: the handshake is `.await`-shaped
/// end to end and has to be able to sit waiting for a host-key or password
/// dialog while the UI keeps drawing.
pub fn service_connect(app: &mut App, remote_tx: &mpsc::Sender<RemoteEvent>) {
    let Some(request) = app.take_pending_connect() else {
        return;
    };
    let registry = Arc::clone(app.remotes());
    let config = app.config.remote.clone();
    let tx = remote_tx.clone();
    tokio::spawn(async move { run_connect(*request, registry, config, tx).await });
}

/// The connect task itself.
///
/// The order is fixed and is the order of the code: dial, verify the host key,
/// authenticate, open the pool, ask the server where we are, register, and
/// only then tell the event loop. Nothing here holds a secret past the call it
/// was made for.
pub async fn run_connect(
    request: ConnectRequest,
    registry: Arc<RemoteRegistry>,
    config: RemoteConfig,
    tx: mpsc::Sender<RemoteEvent>,
) {
    let ConnectRequest {
        answer,
        attempt,
        reconnect,
        ..
    } = request;
    let answer = *answer;
    let target = answer.target;
    let authority = target.authority();
    let store = keyring::store();

    let opened = match target.protocol {
        Protocol::Sftp => {
            connect_sftp(
                target.clone(),
                answer.plan,
                answer.password,
                &config,
                store,
                tx.clone(),
                attempt,
            )
            .await
        }
        Protocol::Ftp | Protocol::Ftps | Protocol::FtpsImplicit => {
            connect_ftp(
                target.clone(),
                answer.plan,
                answer.password,
                config.clone(),
                store,
                tx.clone(),
                attempt,
            )
            .await
        }
    };
    let (transport, start_dir) = match opened {
        Ok(pair) => pair,
        Err(err) => {
            // The message is already phrased for a human and carries no
            // secret: `AuthSequence::failure_message` and the two backends'
            // error mapping are both written to that rule.
            let _ = tx
                .send(RemoteEvent::Failed {
                    attempt,
                    message: err.to_string(),
                })
                .await;
            return;
        }
    };

    // Cloned before the move: the keyring check below needs the
    // target, and `RemoteFs::new` takes it.
    let connected_to = target.clone();
    let fs = RemoteFs::new(
        Target {
            dir: Some(start_dir.clone()),
            ..target
        },
        transport,
        config.listing_ttl.duration(),
    );
    // A reconnect goes back behind the **same** id, so the tab's path does not
    // change.
    let id = match reconnect {
        Some(id) if registry.replace(id, Arc::clone(&fs)) => id,
        _ => match registry.register(Arc::clone(&fs)) {
            Ok(id) => id,
            Err(err) => {
                fs.close();
                let _ = tx
                    .send(RemoteEvent::Failed {
                        attempt,
                        message: err.to_string(),
                    })
                    .await;
                return;
            }
        },
    };
    let start = id.path(&start_dir);
    // the opt-in has two halves and this is the second. A secret in
    // the keyring for this target is only ever there because the user ticked
    // the box, so the host is recorded as `auth = "keyring"` and the next
    // connect actually reads it back. Asked here rather than threaded out of
    // the auth loop because the answer is the same either way and this one
    // also repairs a host whose password was stored before the recording half
    // existed - which was every host, since the tick used to be write-only.
    let saved = keyring::store()
        .get(&connected_to.keyring_account())
        .ok()
        .flatten()
        .is_some()
        .then(|| Box::new(connected_to));
    if tx
        .send(RemoteEvent::Connected {
            attempt,
            id,
            start,
            saved,
        })
        .await
        .is_err()
    {
        // The event loop has gone; do not leave a socket and an actor task
        // behind it.
        registry.close(id);
        let _ = authority;
    }
}

/// the SFTP: russh plus russh-sftp, async end to end.
async fn connect_sftp(
    target: Target,
    plan: crate::remote::auth::AuthPlan,
    password: Option<crate::remote::secret::Secret>,
    config: &RemoteConfig,
    store: Arc<dyn keyring::SecretStore>,
    tx: mpsc::Sender<RemoteEvent>,
    attempt: crate::remote::connect::ConnectId,
) -> Result<(Arc<dyn RemoteTransport>, String)> {
    let file = known_hosts::path()?;
    let hooks = ConnectHooks::new(tx, attempt, file);
    let fs = SftpFs::connect(target, plan, password, config, store, hooks).await?;
    // `SSH_FXP_REALPATH` blocks, and this task is a runtime worker.
    //
    let asking = Arc::clone(&fs);
    let start = tokio::task::spawn_blocking(move || asking.start_dir())
        .await
        .map_err(|err| crate::Error::msg(err.to_string()))??;
    Ok((fs as Arc<dyn RemoteTransport>, start))
}

/// the FTP and FTPS: suppaftp, synchronous, on the blocking pool.
///
/// The whole login runs under `spawn_blocking`, so the password prompt it may
/// raise is asked with `blocking_send` and answered with `blocking_recv` -
/// neither of which is being called from an async context, which is what makes
/// them legal here.
async fn connect_ftp(
    target: Target,
    plan: crate::remote::auth::AuthPlan,
    password: Option<crate::remote::secret::Secret>,
    config: RemoteConfig,
    store: Arc<dyn keyring::SecretStore>,
    tx: mpsc::Sender<RemoteEvent>,
    attempt: crate::remote::connect::ConnectId,
) -> Result<(Arc<dyn RemoteTransport>, String)> {
    let joined = tokio::task::spawn_blocking(move || {
        let hooks = BlockingHooks::new(move |kind, offer_keyring| {
            let (reply, answer) = tokio::sync::oneshot::channel();
            let event = RemoteEvent::Secret {
                attempt,
                kind,
                offer_keyring,
                reply,
            };
            if tx.blocking_send(event).is_err() {
                return None;
            }
            // A dropped sender is a refusal, which is how `Esc` cancels.
            answer.blocking_recv().ok().flatten()
        });
        let fs = FtpFs::connect(target, plan, password, &config, store, hooks)?;
        let start = fs.start_dir().to_string();
        Ok::<_, crate::Error>((fs, start))
    })
    .await
    .map_err(|err| crate::Error::msg(err.to_string()))?;
    let (fs, start) = joined?;
    Ok((fs as Arc<dyn RemoteTransport>, start))
}

/// Queue the rename `Start!` or `Undo` asked for.
///
/// Here rather than in `dispatch` because queueing a job is filesystem work,
/// exactly as the search above is.
pub fn service_rename(app: &mut App) {
    if let Some(request) = app.take_pending_rename() {
        app.start_rename(*request);
    }
}

/// A viewer that has finished opening on the blocking pool.
///
/// A file that will not open is reported in the status line and the panels
/// stay where they are.
pub type OpenedViewer = Result<Viewer>;

/// Start opening the viewer `F3` queued.
///
/// Here rather than in `dispatch` because `dispatch` may not touch the
/// filesystem. **Started**, not finished: a file
/// viewer is built on the blocking pool and arrives through `view_tx`, because
/// `Viewer::open` reads its first window before it returns and on a streamed
/// backend that read is a function of where the member sits in the container -
/// which is exactly the wait the design says must not freeze the UI.
///
/// A generated page - the `F1` help above all - is text that is already in
/// memory, so it is built here and pushed on this frame: sending it round a
/// channel would cost it a frame for nothing.
pub fn open_pending_viewer(
    app: &mut App,
    view_tx: &mpsc::Sender<OpenedViewer>,
    index_tx: &mpsc::Sender<viewer::index::IndexBatch>,
    opening: &mut bool,
) {
    let Some(request) = app.take_pending_view() else {
        return;
    };
    let id = app.next_viewer_id();
    let cfg = app.config.viewer.clone();
    // "Mode is remembered per session and `viewer.default_mode`
    // sets the initial one. A file detected as binary opens in hex
    // automatically." All three, in that precedence, live in `initial_mode`.
    // Only for a **file**: a generated page - the `F1` help, above all - is
    // text that happens to be in memory, and showing it as hex because the
    // last file was binary would be obeying the letter of a rule about files.
    let remembered = app.viewers.mode;
    // the session pattern, cloned out before the panel is
    // borrowed for the blocking open. `None` when nothing has searched yet.
    let seed = app.viewers.last_find.clone();
    match request {
        ViewRequest::File { path, at } => {
            let vfs = Arc::clone(&app.vfs);
            let tx = view_tx.clone();
            *opening = true;
            let built = tokio::task::spawn_blocking(move || {
                Viewer::open_path(id, vfs, path, &cfg).and_then(|mut v| {
                    let binary = v.status().binary;
                    v.set_mode(viewer::hex::initial_mode(
                        cfg.default_mode,
                        remembered,
                        binary,
                    ))?;
                    // a content match opens at the hit with the
                    // pattern that found it installed. After `set_mode`,
                    // because the mode decides how a position is laid out and
                    // placing the cursor first would put it back at the top.
                    match at {
                        Some(start) => v.open_at_hit(start.start, start.find)?,
                        // no hit to open at, so the session's
                        // last pattern is installed instead. Compiled, so the
                        // matches in the window that is about to be drawn are
                        // highlighted, but **not scanned**: the counter behind
                        // it is what costs a pass over the file, and it waits
                        // for the `F3` that asks for it.
                        None => {
                            if let Some(query) = seed {
                                v.seed_find(query);
                            }
                        }
                    }
                    Ok(v)
                })
            });
            // Forwarded by a task rather than sent from inside the blocking
            // one, so that **something** always arrives. The loop swallows
            // keystrokes until it does (they belong to the viewer being
            // opened), and a build that ended without sending - a panic on a
            // file the decoder did not survive - would leave the application
            // drawing frames and answering nothing at all. A `JoinError` is a
            // failure to open like any other.
            tokio::spawn(async move {
                let opened = match built.await {
                    Ok(opened) => opened,
                    Err(join) => Err(crate::error::Error::msg(format!(
                        "the file could not be opened: {join}"
                    ))),
                };
                // A closed channel means the application is going away; there
                // is nothing to report it to.
                let _ = tx.send(opened).await;
            });
        }
        // the reference is generated here rather than in
        // `dispatch`, because it is built from the live keymap, the live
        // column order and the terminal's keyboard capability - thirty pages
        // of text that a keystroke has no business rendering.
        //
        ViewRequest::Help { topic } => {
            let (body, at) = crate::ui::help::page(app, topic);
            let opened =
                Viewer::open_memory(id, "Help".to_string(), body, &cfg).and_then(|mut v| {
                    v.mark_help();
                    // The topic is a line in the one document, not a document
                    // of its own: the design requires quick find to work
                    // across the whole reference, which it cannot do if each
                    // section is a separate viewer.
                    if let Some(line) = at {
                        v.open_at_hit(viewer::HitStart::Line(line), None)?;
                    }
                    Ok(v)
                });
            push_opened_viewer(app, opened, index_tx);
        }
        ViewRequest::Text { title, body, help } => {
            let opened = Viewer::open_memory(id, title, body, &cfg).map(|mut v| {
                if help {
                    v.mark_help();
                }
                v
            });
            push_opened_viewer(app, opened, index_tx);
        }
    }
}

/// Put a viewer that has finished opening on screen, and start its line index.
pub fn push_opened_viewer(
    app: &mut App,
    opened: OpenedViewer,
    index_tx: &mpsc::Sender<viewer::index::IndexBatch>,
) {
    match opened {
        Ok(mut v) => {
            // The scan runs on the blocking pool: it is a `read` loop over a
            // file that may be 40 GB, and that must never be on the path that
            // draws a frame.
            if let Some(job) = v.take_scan() {
                let tx = index_tx.clone();
                let ScanJob {
                    id,
                    source,
                    chunk,
                    cancel,
                } = job;
                tokio::task::spawn_blocking(move || {
                    viewer::index::scan(id, source, chunk, cancel, tx);
                });
            }
            app.push_viewer(v);
        }
        Err(err) => app.message = Some(err.to_string()),
    }
}

/// Perform the copy `Ctrl+C` queued.
///
/// Here rather than in `dispatch` for the same reason [`open_pending_viewer`]
/// is here: it reads the file, and `dispatch` may not touch the filesystem.
/// Called immediately after `open_pending_viewer`
/// and before the layout, so the message it leaves is on the same frame as the
/// keystroke that asked for it.
///
/// The text goes to `OSC 52` **and** to the internal clipboard, always. Whether
/// the terminal took the sequence cannot be known in band - one that ignores it
/// says nothing - so filling the fallback every time is what makes "told about
/// once, not on every copy" a promise the program can actually keep.
///
pub fn service_viewer_copy(app: &mut App) {
    let Some(what) = app.take_viewer_copy() else {
        return;
    };
    let copy_max = app.config.viewer.copy_max.bytes();
    let Some(viewer) = app.viewer_mut() else {
        return;
    };
    // The borrow of the viewer ends here: the report and the clipboard are the
    // application's, not the viewer's.
    let outcome = viewer.copy(what, copy_max);
    match outcome {
        // A refusal is a rule, not a fault: the design refuses rather than
        // truncating, and it says so in the status line like any other answer.
        Ok(viewer::copy::Copied::Refused(why)) => app.message = Some(why),
        Ok(viewer::copy::Copied::Text { text, bytes, note }) => {
            // A write that failed is a write that did not deliver, and it is
            // not worth losing the copy over: the internal clipboard still has
            // the text, and the message says so.
            let delivered = term::osc52::write(&text).unwrap_or_default();
            app.text_clipboard = Some(text);
            app.message = Some(app.copy_report(bytes, note, delivered));
        }
        Err(err) => app.message = Some(format!("copy failed: {err}")),
    }
}

/// Run one background match counter on the blocking pool.
///
/// The blocking pool for the same reason [`viewer::index::scan`] is there: it
/// is a read loop over a file that may be 40 GB, and it must never be on the
/// path that draws a frame.
pub fn spawn_find(job: viewer::find::FindJob, tx: mpsc::Sender<viewer::find::FindBatch>) {
    let viewer::find::FindJob {
        id,
        generation,
        source,
        matcher,
        chunk,
        cancel,
    } = job;
    tokio::task::spawn_blocking(move || {
        viewer::find::scan(id, generation, source, matcher, chunk, cancel, tx);
    });
}

/// Record whether `Shift` is being held, for the key-bar swap.
///
/// Only a *bare* `Shift` press drives this. The enhanced keyboard protocol
/// reports the modifier keys themselves, with press and release event types;
/// a legacy terminal reports neither, so the flag stays false and the key
/// bar keeps its unshifted labels - which is what to do when the terminal
/// cannot report.
///
/// Deliberately *not* driven by `KeyModifiers` on ordinary keys in the setting
/// direction: typing a capital letter into the quick-search buffer would
/// otherwise flip the key bar on every keystroke. Presses and releases of the
/// modifier keys themselves are what set it.
///
/// # Why the release event is not trusted on its own
///
/// It frequently does not arrive. Reporting a bare modifier's release needs
/// both `REPORT_ALL_KEYS_AS_ESCAPE_CODES` and `REPORT_EVENT_TYPES`, and a
/// terminal is free to honour part of the pushed flag set and ignore the rest -
/// so a terminal can report the *press* and never the matching release, which
/// leaves the key bar stuck in a modified layer forever. Releasing a modifier
/// while the window is unfocused produces no release event on any terminal, for
/// the same reason a keyboard state machine cannot see what happens elsewhere.
///
/// So the state is also reconciled **downwards** against every ordinary key:
/// crossterm reports the modifier set that was live for that key, and a
/// modifier absent from it is demonstrably not held. This can only ever clear a
/// stale bit, never set one, so it cannot reintroduce the per-keystroke flicker
/// the paragraph above avoids - and it means the key bar rights itself on the
/// very next keypress instead of needing the application restarted.
pub fn track_modifiers(app: &mut App, key: crossterm::event::KeyEvent) {
    use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers, ModifierKeyCode};

    let bit = match key.code {
        KeyCode::Modifier(ModifierKeyCode::LeftShift | ModifierKeyCode::RightShift) => {
            Some(KeyModifiers::SHIFT)
        }
        KeyCode::Modifier(ModifierKeyCode::LeftControl | ModifierKeyCode::RightControl) => {
            Some(KeyModifiers::CONTROL)
        }
        KeyCode::Modifier(ModifierKeyCode::LeftAlt | ModifierKeyCode::RightAlt) => {
            Some(KeyModifiers::ALT)
        }
        _ => None,
    };

    match bit {
        Some(bit) => app
            .keyboard
            .note_modifier(bit, !matches!(key.kind, KeyEventKind::Release)),
        // An ordinary key: keep only the modifiers it agrees are down.
        None => app.keyboard.note_key(key.modifiers),
    }
}

/// Insert a bracketed paste at the command-line caret.
///
/// Bracketed paste exists so a pasted path is text rather than a burst of
/// navigation keys, and the command line is where a path is being pasted *to* -
/// its caret is persistent state, so this works from panel
/// focus exactly as `Ctrl+Enter` does, and like `Ctrl+Enter` it does not move
/// focus. A modal dialog owns all input, so a paste arriving
/// there is dropped rather than written somewhere the user cannot see.
pub fn paste(app: &mut App, raw: &str) {
    // A modal dialog owns all input, so a paste arriving there is
    // dropped rather than written somewhere the user cannot see. The console is
    // a paste target like any other: the design gives the shell a real terminal,
    // and `App::paste_into_cmdline` passes the bracketing on when it asked for
    // it.
    if !matches!(
        app.focus,
        Focus::Panel(_) | Focus::CommandLine | Focus::Console
    ) {
        return;
    }
    let text = term::paste_text(raw);
    app.paste_into_cmdline(&text);
}

/// Start the shell, or report why not.
///
/// A shell that will not start is **not fatal**: the panels are still a file
/// manager, the v0.1 command line takes over
/// (`App::console_owns_cmdline` is what every path asks), and the reason is on
/// the status line rather than in a crash.
pub fn start_console(app: &mut App, term: &mut Term, tx: &mpsc::Sender<ConsoleEvent>) {
    if !app.config.console.enabled {
        app.set_console(None);
        return;
    }
    let size = term.terminal().size().unwrap_or_default();
    let cwd = app
        .active_panel()
        .active_tab()
        .path
        .local_path()
        .map(std::path::Path::to_path_buf)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| std::path::PathBuf::from("/"));

    match Console::spawn(
        &app.config.console,
        &cwd,
        (size.height, size.width),
        tx.clone(),
    ) {
        Ok(console) => app.set_console(Some(console)),
        Err(err) => {
            app.set_console(None);
            app.message = Some(format!("no console: {err}"));
            app.warnings.push(format!("console: {err}"));
        }
    }
}
