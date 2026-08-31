//! Top-level state and focus arbitration.
//!
//! # Headless by construction
//!
//! [`App`] must be constructible and drivable with **no terminal attached**:
//! [`App::headless`] builds one from compiled-in defaults, and
//! [`crate::input::dispatch`] mutates it without touching stdout, the terminal
//! size, or the filesystem. Anything that needs the outside world is *queued* -
//! a directory read becomes a [`ReadRequest`], a config reload becomes a flag -
//! and the event loop in `main` services the queue. That is what lets the input
//! model be unit-tested by pushing synthetic key events through and asserting on
//! state.

pub mod caret;
pub mod clipboard;
pub mod compare;
pub mod console;
pub mod cursor;
pub mod dialogs;
pub mod drives;
pub mod fileinfo;
pub mod jobs;
pub mod links;
pub mod navigate;
pub mod open;
pub mod quickview;
pub mod reads;
pub mod remote;
pub mod rename;
pub mod resize;
pub mod search;
pub mod settings;
pub mod update;
pub mod viewer;

use std::collections::HashMap;
use std::sync::Arc;

use crate::config::{ColorDepth, Config, Keymap, Loaded, Theme, start_dir};
use crate::dialog::DialogFrame;
use crate::error::Result;
use crate::input::{CommandLine, Focus};
use crate::ops::clipboard::Clipboard;
use crate::panel::{Panel, Side, VirtualKind};
use crate::remote::auth::SecretKind;
use crate::remote::connect::ConnectId;
use crate::remote::{RemoteId, Target};
use crate::search::{Query, walk::Tally};
use crate::ui::quickview::QuickView;
use crate::vfs::{
    ArchiveSession, BackendKind, Entry, READ_DIR_BATCH, READ_DIR_FLUSH, Vfs, VfsPath, VfsRouter,
    archive::format::FormatId, list::ListingId,
};
use tokio::sync::mpsc;

/// A directory read that [`crate::input::dispatch`] asked for and the event loop
/// has not started yet.
#[derive(Debug, Clone)]
pub struct ReadRequest {
    /// Which panel asked.
    pub side: Side,
    /// Which of that panel's tabs.
    pub tab: usize,
    /// Bumped every time; a [`VfsEvent`] carrying an older generation is stale.
    pub generation: u64,
    /// What to read.
    pub path: VfsPath,
}

/// A container being entered, remembered until its first listing says whether
/// it really was one.
///
/// the design detects archives "by content sniffing first, extension
/// second", and the design says the same of a disk image: an extension is
/// a hint and never the answer. Reading a file's first block is filesystem
/// access that `dispatch` may not perform. So `Enter`
/// pushes the container segment optimistically and this is what makes the
/// optimism safe: when the read fails before producing a single row - the
/// `.zip` that is really HTML - the panel goes back where it was, with the
/// reason.
#[derive(Debug, Clone)]
struct ContainerAttempt {
    /// Which panel.
    side: Side,
    /// Which of its tabs.
    tab: usize,
    /// The directory the panel was showing, to go back to.
    from: VfsPath,
    /// The container's own file name, to land the cursor back on it.
    name: String,
    /// The path of the file that was entered, so `Ctrl+PgDn`'s retry addresses
    /// the same bytes the first attempt did.
    ///
    /// Not `from.join(name)`: a row of a virtual listing carries its real home
    /// in `Entry::location` and that is where a search result actually lives,
    /// so the path is recorded rather than recomputed.
    container: VfsPath,
    /// Which kind of segment was pushed, so a failure can say what was tried
    /// and a retry can pick the other one.
    tried: BackendKind,
    /// Whether one more kind is still owed before the panel goes back.
    ///
    /// Set only by `Ctrl+PgDn`, which the design makes the key that ignores
    /// the extension: a failed [`BackendKind::Archive`] attempt is entered
    /// again as [`BackendKind::Image`] before the panel is restored, so a
    /// `backup.dat` that is really a disk image opens. `Enter` decides by name
    /// and never retries.
    retry: bool,
}

/// A connection the event loop has not started yet.
///
/// Queued rather than performed, because connecting is I/O and `dispatch` may
/// not do any.
#[derive(Debug)]
pub struct ConnectRequest {
    /// Everything the connect dialog collected.
    pub answer: Box<crate::dialog::ConnectAnswer>,
    /// Which panel is connecting.
    pub side: Side,
    /// Which of its tabs.
    pub tab: usize,
    /// The local path the tab is on now, remembered so disconnecting restores
    /// it.
    pub origin: VfsPath,
    /// The name the cursor was on there, so disconnecting lands back on it.
    pub origin_cursor: Option<String>,
    /// The id this attempt answers to.
    pub attempt: ConnectId,
    /// A reconnect of an existing tab, which keeps the same
    /// [`RemoteId`] so the tab's path does not change.
    pub reconnect: Option<RemoteId>,
}

/// What a running connect task tells the event loop.
///
/// Every question carries its own reply channel, and **dropping the sender is
/// a refusal**: that is how `Esc` cancels a connect with no extra code path.
///
#[derive(Debug)]
pub enum RemoteEvent {
    /// An unknown host key. The answer goes back through `reply`.
    HostKey {
        /// Which attempt is asking.
        attempt: ConnectId,
        /// Which host. Secret-free.
        target: Target,
        /// `SHA256:...`, exactly as `ssh-keygen -l` renders it.
        fingerprint: String,
        /// `true` accepts and learns the key; anything else refuses.
        reply: tokio::sync::oneshot::Sender<bool>,
    },
    /// A **changed** host key.
    ///
    /// There is no `reply` field, so there is no code path that can accept
    /// one: the "do not offer a one-key override" is enforced by
    /// this type and not by a policy check somebody could invert (S6).
    HostKeyChanged {
        /// Which attempt.
        attempt: ConnectId,
        /// Which host.
        target: Target,
        /// The key the server offered now.
        fingerprint: String,
        /// Which line of `known_hosts` records the other one.
        line: usize,
        /// Which file.
        file: std::path::PathBuf,
    },
    /// A password or a passphrase.
    Secret {
        /// Which attempt.
        attempt: ConnectId,
        /// Which question to put on the screen.
        kind: SecretKind,
        /// Whether to offer "Save in the system keyring" - only when the host
        /// opted in **and** a store exists.
        offer_keyring: bool,
        /// The answer, or `None` for every way of not answering.
        reply: tokio::sync::oneshot::Sender<Option<crate::dialog::SecretAnswer>>,
    },
    /// Connected. The tab moves to `start` and reads.
    Connected {
        /// Which attempt.
        attempt: ConnectId,
        /// The id its paths name.
        id: RemoteId,
        /// Where the panel lands.
        start: VfsPath,
        /// The password was stored in the keyring for this target.
        ///
        ///
        /// The other half of the opt-in: the secret is in the keyring, and the
        /// host has to be told it may be used or nothing will ever read it
        /// back. Carries the target rather than a secret - **never a secret**,
        /// like every other field on this enum.
        saved: Option<Box<crate::remote::Target>>,
    },
    /// Not connected, and why. **Never carries a secret.**
    Failed {
        /// Which attempt.
        attempt: ConnectId,
        /// Already phrased for the status line.
        message: String,
    },
}

/// A result from a running directory read, delivered back to the UI thread.
#[derive(Debug, Clone)]
pub enum VfsEvent {
    /// Some entries arrived. The panel renders them immediately.
    Entries {
        /// Which panel.
        side: Side,
        /// Which tab.
        tab: usize,
        /// Which read.
        generation: u64,
        /// The batch.
        batch: Vec<Entry>,
    },
    /// The listing is complete.
    Done {
        /// Which panel.
        side: Side,
        /// Which tab.
        tab: usize,
        /// Which read.
        generation: u64,
    },
    /// The listing failed. The panel keeps whatever arrived and shows why.
    Failed {
        /// Which panel.
        side: Side,
        /// Which tab.
        tab: usize,
        /// Which read.
        generation: u64,
        /// Already phrased for a human.
        message: String,
    },
}

impl VfsEvent {
    /// The panel this event belongs to.
    pub const fn side(&self) -> Side {
        match self {
            Self::Entries { side, .. } | Self::Done { side, .. } | Self::Failed { side, .. } => {
                *side
            }
        }
    }
}

/// Drain one directory listing into [`VfsEvent`]s, batching so the UI repaints
/// promptly on a big directory without one message per file.
///
/// Here rather than in `main` because of what it is the last hop of: the search
/// results reach the panel through this function and nothing else, so "results
/// stream back over a channel as they are found" is a property of this loop and
/// has to be assertable without a terminal, a server and a stopwatch.
///
/// # A batch is bounded by [`READ_DIR_FLUSH`] as well as by [`READ_DIR_BATCH`]
///
/// A directory read ends, so its last partial batch is sent the moment the
/// backend closes the channel. A search listing does not end until the walk
/// does, and over a network that is minutes: waiting for 128 hits before
/// showing the first one is what left a remote search counting hits in the
/// status line - which reads the [`ListFs`] directly - above a panel with
/// nothing in it. So a batch that has been waiting is sent as it stands.
///
/// # The caller spawns it, and the caller cancels it
///
/// It is one `await` chain and holds no state the event loop can see, so the
/// event loop keeps its [`tokio::task::AbortHandle`] instead
/// ([`App::register_read`]). Aborting drops this future and with it the
/// receiver `read_dir` returned, the backend's next `send` fails, and the walk
/// stops - the cancellation [`crate::vfs`] documents. The generation check in
/// [`App::apply_vfs_event`] is still what keeps a batch already in flight from
/// being drawn under the wrong path; the two answer different halves of a
/// superseded read.
pub async fn stream_read(vfs: Arc<dyn Vfs>, request: ReadRequest, tx: mpsc::Sender<VfsEvent>) {
    let ReadRequest {
        side,
        tab,
        generation,
        path,
    } = request;
    let mut rx = vfs.read_dir(&path);
    let mut batch = Vec::with_capacity(READ_DIR_BATCH);

    loop {
        // An empty batch waits with no timer at all, so a panel sitting on a
        // finished search costs exactly what it did before.
        let next = if batch.is_empty() {
            rx.recv().await
        } else {
            match tokio::time::timeout(READ_DIR_FLUSH, rx.recv()).await {
                Ok(next) => next,
                Err(_) => {
                    let send = std::mem::take(&mut batch);
                    if !send_entries(&tx, side, tab, generation, send).await {
                        return;
                    }
                    continue;
                }
            }
        };
        let Some(item) = next else {
            break;
        };
        match item {
            Ok(entry) => batch.push(entry),
            Err(err) => {
                // Flush what has already been read before reporting the
                // failure. `VfsEvent::Failed` is documented as "the panel
                // keeps whatever arrived and shows why", and dropping the
                // pending batch broke that - on an unreadable directory the
                // only thing in it is the `..` row the backend sends first,
                // so the panel lost the one row that could get the user out.
                //
                if !batch.is_empty() {
                    let send = std::mem::take(&mut batch);
                    // Whether anyone is still listening is answered by the
                    // `Failed` send below, which is the one that has to go
                    // out; this one is best effort by construction.
                    send_entries(&tx, side, tab, generation, send).await;
                }
                let _ = tx
                    .send(VfsEvent::Failed {
                        side,
                        tab,
                        generation,
                        message: err.to_string(),
                    })
                    .await;
                return;
            }
        }
        if batch.len() >= READ_DIR_BATCH {
            let send = std::mem::take(&mut batch);
            if !send_entries(&tx, side, tab, generation, send).await {
                return;
            }
        }
    }

    if !batch.is_empty() && !send_entries(&tx, side, tab, generation, batch).await {
        return;
    }
    let _ = tx
        .send(VfsEvent::Done {
            side,
            tab,
            generation,
        })
        .await;
}

/// Hand one batch of rows to the event loop.
///
/// `false` once the receiver has gone, which is the event loop shutting down
/// and the one reason [`stream_read`] stops early.
async fn send_entries(
    tx: &mpsc::Sender<VfsEvent>,
    side: Side,
    tab: usize,
    generation: u64,
    batch: Vec<Entry>,
) -> bool {
    tx.send(VfsEvent::Entries {
        side,
        tab,
        generation,
        batch,
    })
    .await
    .is_ok()
}

/// A program to run outside the alternate screen (the `F4`, and
/// the `execute_in = "detached"` when that lands).
///
/// Queued by [`crate::input::dispatch`] and performed by the event loop, which
/// is the only place that may touch the terminal. `follow` is the panel to
/// re-read afterwards - the design ends its sequence with "force full redraw
/// → reread the panel", and the editor may have created the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalCommand {
    /// The program, already resolved from `editor.command`, `$VISUAL` or
    /// `$EDITOR`.
    pub program: String,
    /// Its arguments, with `{file}` and `{line}` already substituted.
    pub args: Vec<String>,
    /// The working directory to run it in, when it matters.
    pub cwd: Option<VfsPath>,
    /// Re-read this panel once the program exits.
    pub follow: Option<Side>,
}

/// A file the user asked to view, which the event loop has not opened yet.
///
///
/// Queued for the same reason a [`ReadRequest`] is: opening a viewer reads from
/// the filesystem and spawns a background scan, and
/// [`crate::input::dispatch`] does neither. `F3` records what to open and the
/// event loop opens it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewRequest {
    /// A path on the [`Vfs`], optionally opened somewhere other than the start.
    ///
    File {
        /// What to open.
        path: VfsPath,
        /// Where to open it, when a search hit asked rather than `F3`.
        at: Option<Box<ViewerStart>>,
    },
    /// Two files, shown as a unified diff.
    ///
    /// Both are read by the event loop, because reading them is I/O and
    /// `dispatch` performs none. `old` is the left-hand side of the diff and
    /// `new` is the file the viewer is opened over, so `1` and `2` show the
    /// new one's own text and bytes.
    Diff {
        /// The side the diff calls `---`.
        old: VfsPath,
        /// The side the diff calls `+++`, and the file the viewer holds.
        new: VfsPath,
    },
    /// the whole-program reference, on the section the key was
    /// pressed in.
    ///
    /// A topic rather than a body, because the page is **generated from the
    /// live keymap and the live column order** and `dispatch` has an `&mut
    /// App` but no business rendering thirty pages of text on a keystroke.
    /// The event loop calls
    /// [`crate::ui::help::page`] with `&App` and opens the viewer over the
    /// result.
    Help {
        /// Which section to land on.
        topic: crate::ui::help::HelpTopic,
    },
    /// Text that was generated rather than read - the `F1` viewer page,
    /// which "uses the same viewer machinery".
    Text {
        /// What to call it.
        title: String,
        /// The whole page. Generated, so it is already in memory and is not a
        /// file being read whole.
        body: String,
        /// True for an `F1` page, so `F1` pressed *on* it does not stack a
        /// second copy.
        help: bool,
    },
}

/// Where a viewer opens, when something other than byte 0 asked for it.
///
///
/// > For a content match, `Enter` opens the viewer at the matching line with
/// > the hit already highlighted - by the same `grep-regex` matcher that found
/// > it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewerStart {
    /// Where the cursor is placed.
    ///
    /// A byte offset wherever the searcher had one, because the design
    /// makes position a byte offset and a line number approximate until the
    /// index has finished building - and the viewer opens before it has. A
    /// hit found in one of the transcoded charsets has no file
    /// offset to give and travels as its line instead
    /// ([`crate::vfs::ContentHit::decoded`]).
    pub start: crate::viewer::HitStart,
    /// For the message, when the searcher counted lines.
    pub line: Option<u64>,
    /// The find query to install, so the hit is highlighted by the pattern
    /// that found it and `n` steps through the rest.
    ///
    /// `None` for a regex content search, which the viewer's find bar cannot
    /// compile yet: it matches over overlapping windows and the overlap rule
    /// needs a bounded match length, which a regex does not have
    /// (`crate::viewer::find::REGEX_MILESTONE`). The viewer still opens at the
    /// hit and the status line says so.
    pub find: Option<crate::viewer::find::FindQuery>,
}

/// A search the event loop has not started yet.
///
/// Queued for the same reason a [`ReadRequest`] is: compiling a pattern is
/// cheap, but registering a listing, spawning the walk and re-pointing the tab
/// all touch state the event loop owns, and `dispatch` may not read.
///
#[derive(Debug, Clone)]
pub struct SearchRequest {
    /// Which panel the results go into: **the panel that was active when the
    /// search started**.
    pub side: Side,
    /// Which of its tabs, recorded now so a tab switch between the keystroke
    /// and the frame cannot redirect the results.
    pub tab: usize,
    /// What to look for.
    pub query: Query,
    /// `Alt+F7` or `Ctrl+B`.
    pub kind: VirtualKind,
}

/// A walk that [`App::start_search`] has spawned.
///
/// Held by the event loop, which is the only place that can wait for it. Its
/// [`Tally`] is what the honesty rule needs at the end of a search:
/// a walk that could not read three directories says so once, and dropping
/// this handle would be choosing not to say it.
#[derive(Debug)]
pub struct StartedSearch {
    /// The panel the results are going into.
    pub side: Side,
    /// Which listing, so a tally that arrives after the panel has moved on to
    /// a second search is not reported against the first one's rows.
    pub listing: ListingId,
    /// The blocking task doing the walk. Yields the [`Tally`] when it ends.
    pub walk: tokio::task::JoinHandle<Tally>,
}

/// A rename the event loop has not queued yet.
///
/// Queued for the same reason a [`SearchRequest`] is: `Start!` is answered
/// inside `dispatch`, and turning a list of pairs into a job touches the queue
/// and the filesystem, which `dispatch` may not.
#[derive(Debug, Clone)]
pub struct RenameRequest {
    /// Every rename the plan produced, in the order the preview showed them.
    pub pairs: Vec<(VfsPath, VfsPath)>,
    /// True when this is the `Undo` button rather than `Start!`, so the message
    /// says which and the undo store is cleared rather than replaced.
    pub undoing: bool,
}

/// Everything [`App::show_listing`] needs that is not the listing itself.
///
/// A struct rather than eight arguments, because six of these are the
/// [`VirtualView`] the tab ends up holding and the seventh is the one it is
/// replacing - which is one idea, not a parameter list.
pub(crate) struct PendingView {
    pub(crate) kind: VirtualKind,
    pub(crate) header: String,
    pub(crate) find: Option<crate::viewer::find::FindQuery>,
    pub(crate) origin: VfsPath,
    pub(crate) origin_cursor: Option<String>,
    /// The listing this tab was showing, forgotten once the new one is
    /// registered.
    pub(crate) previous: Option<ListingId>,
}

/// A summary of where keyboard input goes, for [`App::input_route`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputRoute {
    focus: Focus,
    viewer: bool,
    pending_view: bool,
    /// A queued copy, so `main::drain_input` stops on it exactly as it stops on
    /// a queued view.
    ///
    /// Without it `Ctrl+C`, `Esc`, `Ctrl+C` collapse into one frame and the
    /// second copy is made from a selection the first one still had.
    viewer_copy: bool,
    /// A queued search, so `drain_input` stops on it exactly as it stops on a
    /// queued view: `Alt+F7`, `Esc`, `Alt+F7` must not collapse into one
    /// frame, or the `Esc` would cancel a listing that does not exist yet.
    search: bool,
    dialog: bool,
    console_restart: bool,
    quit: bool,
    /// Whether a quick view is showing, so `main::drain_input` stops on the
    /// frame `Ctrl+Q` changed it: the key after it belongs to a panel that is
    /// either showing a listing or showing a file, and which one decides where
    /// it goes.
    quick: bool,
    /// Whether a device popup is queued, so `main::drain_input` stops on the
    /// frame `Alt+F1` asked for one: the key after it belongs to a dialog that
    /// does not exist until the event loop has read the mount table.
    ///
    drives: bool,
    /// Whether an open is queued, so `main::drain_input` stops on the frame
    /// `Enter` asked for one: the key after it may belong to the design's
    /// execute prompt, which does not exist until the event loop has read the
    /// file's head.
    open: bool,
}

/// What `Alt+F5` left for the event loop.
///
/// Every answer the dialog collected, plus the resolved container path. It is
/// a request rather than an operation because creating the archive is
/// filesystem work and `dispatch` may not do any
/// - the same shape as `F3`'s queued viewer and
///   `F5`'s queued job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackRequest {
    /// The archive to create, resolved against the panel.
    pub container: VfsPath,
    /// Which format to write.
    pub format: crate::vfs::archive::format::FormatId,
    /// `0..=9`, rescaled per format when the container is written.
    pub level: u8,
    /// What goes into it.
    pub sources: Vec<VfsPath>,
    /// "Move to archive": delete the sources, and only after a copy that
    /// succeeded.
    pub move_sources: bool,
}

/// Everything the application knows.
pub struct App {
    /// `config.toml`, live - `Ctrl+H` and `Ctrl+Alt+R` both mutate it.
    pub config: Config,
    /// The resolved keymap.
    pub keymap: Keymap,
    /// The resolved theme.
    pub theme: Theme,
    /// How many colours this session can show. Detected once at
    /// startup; `TrueColor` in a headless app so tests are deterministic.
    pub color_depth: ColorDepth,
    /// The left panel.
    pub left: Panel,
    /// The right panel.
    pub right: Panel,
    /// Where keys go.
    pub focus: Focus,
    /// Which panel is *active*, independent of whether it has focus.
    ///
    /// The command line acts on the active panel - `Ctrl+Enter` inserts the
    /// entry under *its* cursor - so this survives focus moving to the command
    /// line. Always kept in step with `focus` by
    /// [`App::set_focus`].
    pub active_side: Side,
    /// The command line, including its persistent caret.
    pub cmdline: CommandLine,
    /// The backend both panels read through.
    ///
    /// v0.1 was local-only; v0.5 makes it a [`VfsRouter`], which picks the
    /// backend per path so that a panel inside an archive is read by
    /// [`crate::vfs::ArchiveFs`] and everything else by [`crate::vfs::LocalFs`].
    /// Nothing that holds this field had to change, which
    /// is the claim the design makes for the trait.
    pub vfs: Arc<dyn Vfs>,

    /// The same router, concretely, for the questions the trait cannot answer
    /// without a path: which backend services one ([`App::vfs_for`]) and what
    /// the archive session is ([`App::archive_session`]).
    router: Arc<VfsRouter>,

    /// Containers being entered, by the generation of the read that will say
    /// whether they really were containers.
    ///
    /// the design both detect by content, and content can only
    /// be looked at by reading the file - which `dispatch` may not do. So
    /// `Enter` on a `foo.zip` or a `foo.iso` navigates *optimistically* and
    /// this remembers where the panel came from; a `.zip` that turns out to be
    /// HTML, or an `.img` that is not a disk image, fails the listing before a
    /// single row arrives, and [`App::apply_vfs_event`] puts the panel back
    /// with the reason in the status line.
    container_attempts: HashMap<u64, ContainerAttempt>,
    /// The status-line message, cleared on the next key.
    pub message: Option<String>,
    /// Set by the quit action; the event loop reads it.
    pub should_quit: bool,
    /// What the terminal's keyboard protocol has reported.
    pub keyboard: crate::input::Keyboard,
    /// What `Ctrl+C` / `Ctrl+X` remembered.
    ///
    /// Paths, not bytes, so it costs nothing to hold and survives navigation,
    /// tab switches and panel switches. Lasts the session.
    pub clipboard: Option<Clipboard>,
    /// the fallback store: **text**, not paths.
    ///
    /// > the selection goes to the internal clipboard so it can at
    /// > least be pasted into the command line
    ///
    /// the clipboard "holds **paths, not bytes**" and its `Ctrl+V`
    /// copies files into a directory; it cannot hold a string, so this is the
    /// sibling that can. It is what `Ctrl+V` inserts **at the command line**,
    /// and a `Ctrl+C` on a panel clears it - the same key has just said
    /// "remember this instead", and the command line should paste the more
    /// recent of the two.
    ///
    /// Bounded by `viewer.copy_max`, because that is what filled it.
    pub text_clipboard: Option<String>,
    /// a terminal without `OSC 52` is "told about once, not on
    /// every copy". One session, one telling.
    ///
    /// Support cannot be detected in band - a terminal that ignores the
    /// sequence says nothing - so what is kept is the promise the user can
    /// observe rather than a detection nobody can make.
    ///
    pub osc52_notice_shown: bool,

    /// What the `+` / `-` prompt and the copy dialog's `+ F8` last heard.
    ///
    pub masks: crate::panel::mask::History,
    /// Warnings from configuration loading, shown once.
    pub warnings: Vec<String>,
    /// Set by `Ctrl+Alt+R`; the event loop performs the reload, so `dispatch`
    /// stays free of filesystem access.
    pub reload_requested: bool,

    /// Every job this session has handed out an id for.
    pub jobs: crate::app::jobs::registry::Jobs,

    /// The file operation the user has started describing and not yet
    /// confirmed.
    pub draft: crate::app::jobs::draft::JobDraft,

    /// `Alt+F5`'s answered dialog, waiting for the event loop.
    ///
    /// Queued rather than carried out, for the reason every other filesystem
    /// touch is queued: `dispatch` may not open a file,
    /// and creating the container is opening one.
    pub pending_pack: Option<PackRequest>,

    /// Whether the version check has been asked for, started, or neither.
    ///
    /// Queued by the keystroke and started by the event loop, because
    /// `dispatch` may not touch the network.
    pub update_check: crate::app::update::UpdateCheck,
    /// the warning: which jobs are held by it and which have
    /// already been agreed to.
    rewrite_gate: crate::ops::gate::RewriteGate,

    /// The console's place in the layout, occupied or not.
    pub console: crate::console::Pane,

    /// The modal dialog stack. The last entry is on top and has
    /// focus. Private so the focus invariant lives in one place; see
    /// [`App::push_dialog`].
    dialogs: Vec<DialogFrame>,

    /// Directory reads asked for and not yet started.
    pending_reads: Vec<ReadRequest>,
    /// The task streaming each tab's listing, keyed by panel and tab index.
    ///
    /// A tab has one live read at a time, so starting a read supersedes the
    /// one that was still filling that tab. The generation check in
    /// [`App::apply_vfs_event`] stops the superseded rows being *drawn*;
    /// without a handle here nothing stopped them being *produced*, and
    /// walking through five large directories left five walks running. See
    /// [`App::register_read`].
    read_tasks: HashMap<(Side, usize), tokio::task::AbortHandle>,
    /// Monotonic source for [`ReadRequest::generation`].
    generation: u64,
    /// The open viewers, the geometry they are drawn at, and the focus the
    /// stack displaced.
    pub viewers: crate::viewer::stack::Stack,

    /// What this session remembers about searching. Boxed because it carries
    /// a whole [`Query`] and `App` is moved on every frame.
    pub search: Box<crate::search::Session>,

    /// What this session remembers about renaming.
    pub rename: crate::rename::MultiRename,

    /// The one connect attempt that may be live, and the questions it is
    /// waiting on answers to.
    pub connector: crate::remote::connect::Connector,

    /// `hosts.toml`, read once at startup the way `searches.toml` is.
    ///
    pub hosts: crate::remote::hosts::Book,

    /// the quick view, when one is showing.
    ///
    /// Boxed because it carries a whole [`Viewer`] and `App` is moved on every
    /// frame, exactly as `search_state` is.
    pub quick: Option<Box<QuickView>>,
    /// The body geometry the last drawn frame measured for the quick view.
    ///
    /// The twin of `viewer_view`, and here for the same reason: a viewer
    /// opened between two frames has to be laid out before the keys waiting
    /// behind it are applied.
    quick_view_geometry: (u16, u16),

    /// `hotlist.toml`, read once at startup the way `hosts` is.
    ///
    pub hotlist: crate::devices::hotlist::Hotlist,
    /// The drive popup's request slot and re-enumeration deadline.
    ///
    pub drives: crate::devices::Drives,
    /// The file this session is about to hand to something outside itself.
    ///
    pub handoff: crate::ops::open::Handoff,
    /// A file-information dialog asked for, waiting for its head to be read.
    ///
    /// Set by the keystroke and taken by the event loop, because recognising
    /// a file's contents means reading the front of it and `dispatch` may not.
    pending_file_info: Option<crate::app::fileinfo::FileInfoRequest>,
    /// Text a keystroke asked to put on the system clipboard.
    ///
    /// Queued rather than written where the key was pressed: the write is an
    /// `OSC 52` sequence to the terminal, and the event loop owns writing to
    /// it. `service_viewer_copy` does the same thing for the viewer's copy.
    pending_clipboard: Option<String>,
    /// A resize dialog asked for, waiting for the first image's header to be
    /// read.
    ///
    /// Set by the keystroke and taken by the event loop, for
    /// [`pending_file_info`](Self::pending_file_info)'s reason: the dialog
    /// names the image's pixel size and only the file's own header has it.
    pending_resize: Option<crate::app::resize::ResizeRequest>,
    /// The two names a running [`crate::ops::JobKind::CompareFiles`] is about.
    ///
    /// Kept because the verdict box names both files and a [`JobSummary`]
    /// carries paths only for what failed. Taken when the verdict is shown, so
    /// a second comparison cannot be described with the first one's names.
    ///
    /// [`JobSummary`]: crate::ops::JobSummary
    compare_names: Option<(String, String)>,
    /// A link the keystroke asked for, waiting for the event loop.
    pending_link: Option<crate::app::links::LinkRequest>,
    /// A permission change the keystroke asked for.
    pending_chmod: Option<crate::app::links::ChmodRequest>,
    /// A directory whose git flags are being computed off the loop.
    pending_git_status: Option<crate::app::reads::GitStatusRequest>,
    /// A password to put in the keyring, from the host form.
    pending_keyring: Option<crate::app::links::KeyringWrite>,
    /// A password typed into the host form, between the form being answered
    /// and the host list being applied. Taken by `host_form_answered`.
    pub pending_host_secret: Option<String>,
    /// True when the host form that was just answered added a host, so the
    /// connect dialog underneath selects the new host rather than staying on
    /// the Add button. Editing leaves the cursor where it was.
    pub pending_new_host: bool,
    /// The file `F4` is about to edit, held while the large-file warning is
    /// on screen. Taken when the warning is answered yes.
    pub editor_size_pending: Option<VfsPath>,
    /// Set for one keystroke after the large-file warning is accepted, so the
    /// retried `F4` opens the editor rather than asking again.
    pub editor_size_confirmed: bool,
}

/// Which popup the design asked for.
///
/// Queued rather than built in `dispatch` for the reason every read is: the
/// list comes from the mount table and from stating each hotlist path, and
/// `dispatch` may touch neither.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrivesRequest {
    /// `Alt+F1` / `Alt+F2`: devices, a separator, then the hotlist, acting on
    /// **this** side whichever panel has focus.
    Devices(Side),
    /// `Ctrl+D`: the hotlist alone, acting on the active panel.
    ///
    Hotlist,
}

/// An `Enter` or `Shift+Enter` the event loop owes an association for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenRequest {
    /// What to open.
    pub path: VfsPath,
    /// True for `Shift+Enter`, which "**always** opens with the associated
    /// application, never executes".
    pub never_execute: bool,
    /// How much of the resolution is still to be done.
    ///
    /// **A third field beyond the two.** The
    /// contract gives `dispatch` no way to say "the prompt was answered" or
    /// "list the applications", and both of those read - the desktop entry
    /// directories in the second case - so both belong on this queue rather
    /// than in a second one. Defaulted by [`OpenRequest::new`], so the two
    /// call sites the contract names read exactly as it writes them.
    pub intent: OpenIntent,
}

impl OpenRequest {
    /// `Enter` or `Shift+Enter`: resolve the association from scratch.
    pub const fn new(path: VfsPath, never_execute: bool) -> Self {
        Self {
            path,
            never_execute,
            intent: OpenIntent::Resolve,
        }
    }
}

/// Which half of the design an [`OpenRequest`] is asking for.
///
/// Every variant reads something, which is why they share one queue that only
/// the event loop drains.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenIntent {
    /// Sniff the head, apply the execute policy, resolve the
    /// association. The answer to `Enter`.
    Resolve,
    /// the prompt was answered `Execute`: run it, whatever
    /// `open.execute` says, because the user has just said so for this file.
    Execute,
    /// Open the chooser over this file. Reads the desktop entry
    /// directories.
    Chooser,
    /// The chooser named an application by its desktop entry id.
    Application(String),
}

impl App {
    /// Build from loaded configuration, with both panels at `start`.
    pub fn new(loaded: Loaded, start: VfsPath) -> Self {
        let Loaded {
            config,
            keymap,
            theme,
            warnings,
            ..
        } = loaded;
        let ops_cfg = config.ops.clone();
        let router = Arc::new(VfsRouter::new(
            config.archive.clone(),
            config.remote.clone(),
        ));
        Self {
            config,
            keymap,
            theme,
            color_depth: ColorDepth::TrueColor,
            left: Panel::new(Side::Left, start.clone()),
            right: Panel::new(Side::Right, start),
            focus: Focus::Panel(Side::Left),
            active_side: Side::Left,
            cmdline: CommandLine::new(),
            vfs: Arc::clone(&router) as Arc<dyn Vfs>,
            router,
            container_attempts: HashMap::new(),
            message: None,
            should_quit: false,
            keyboard: crate::input::Keyboard::default(),
            clipboard: None,
            text_clipboard: None,
            osc52_notice_shown: false,
            masks: crate::panel::mask::History::default(),
            warnings,
            reload_requested: false,
            jobs: crate::app::jobs::registry::Jobs::new(&ops_cfg),
            draft: crate::app::jobs::draft::JobDraft::default(),
            pending_pack: None,
            update_check: crate::app::update::UpdateCheck::default(),
            rewrite_gate: crate::ops::gate::RewriteGate::default(),
            console: crate::console::Pane::default(),
            dialogs: Vec::new(),
            pending_reads: Vec::new(),
            read_tasks: HashMap::new(),
            generation: 0,
            viewers: crate::viewer::stack::Stack::default(),
            search: Box::default(),
            rename: crate::rename::MultiRename::default(),
            connector: crate::remote::connect::Connector::default(),
            hosts: crate::remote::hosts::Book::default(),
            quick: None,
            quick_view_geometry: (0, 0),
            hotlist: crate::devices::hotlist::Hotlist::default(),
            drives: crate::devices::Drives::default(),
            handoff: crate::ops::open::Handoff::default(),
            pending_file_info: None,
            pending_clipboard: None,
            pending_resize: None,
            compare_names: None,
            pending_link: None,
            pending_chmod: None,
            pending_git_status: None,
            pending_keyring: None,
            pending_host_secret: None,
            pending_new_host: false,
            editor_size_pending: None,
            editor_size_confirmed: false,
        }
    }

    /// An app with no terminal and nothing read from disk.
    ///
    /// Both panels sit at `/` with empty listings. This is the constructor the
    /// input tests use: build one, push [`crossterm::event::KeyEvent`]s
    /// through [`crate::input::dispatch`], assert on state.
    pub fn headless(config: Config, keymap: Keymap, theme: Theme) -> Self {
        Self::new(
            Loaded {
                config,
                keymap,
                theme,
                dir: None,
                warnings: Vec::new(),
            },
            VfsPath::local_root(),
        )
    }

    /// The directory a fresh session starts in.
    pub fn default_start() -> VfsPath {
        VfsPath::local(start_dir())
    }

    // ------------------------------------------------------------ panels ----

    /// The panel that commands act on.
    pub fn active_panel(&self) -> &Panel {
        self.panel(self.active_side)
    }

    /// The panel that commands act on, mutably.
    pub fn active_panel_mut(&mut self) -> &mut Panel {
        self.panel_mut(self.active_side)
    }

    /// One panel by side.
    pub fn panel(&self, side: Side) -> &Panel {
        match side {
            Side::Left => &self.left,
            Side::Right => &self.right,
        }
    }

    /// One panel by side, mutably.
    pub fn panel_mut(&mut self, side: Side) -> &mut Panel {
        match side {
            Side::Left => &mut self.left,
            Side::Right => &mut self.right,
        }
    }

    /// Move focus, keeping [`App::active_side`] in step.
    ///
    /// Focus moving to the command line leaves the active side alone, which is
    /// exactly what the design needs: `Ctrl+Enter` from the command line still
    /// inserts the entry under the active panel's cursor.
    pub fn set_focus(&mut self, focus: Focus) {
        let moved = matches!(focus, Focus::Panel(side) if side != self.active_side);
        if let Focus::Panel(side) = focus {
            self.active_side = side;
        }
        // the indicator has been answered: the shell's screen is
        // what the user is now looking at.
        if focus == Focus::Console {
            self.console.activity = false;
        }
        self.focus = focus;
        // "the shell's cwd and the active panel's directory must
        // track each other". `Tab` changes which directory is active without
        // reading one, so it never reaches `navigate_selecting` - and a shell
        // left in the other panel's directory is one `Ctrl+Enter` away from
        // composing a command against a file it cannot see (the design
        // inserts the bare name).
        if moved {
            self.sync_active_cwd();
        }
    }

    /// Tell the shell where the active panel is, whatever route got it there.
    ///
    ///
    /// Idempotent: [`crate::console::sync::CwdSync`] remembers the last
    /// destination and answers `AlreadyThere` for a repeat, so the callers that
    /// change the active directory without reading one - `Tab`, `Alt+<n>`,
    /// `Ctrl+Tab`, `Ctrl+U` - can call it unconditionally.
    pub fn sync_active_cwd(&mut self) {
        let side = self.active_side;
        self.sync_shell_cwd(side);
    }

    /// `Ctrl+U`: swap the two panels' contents, not their identities.
    pub fn swap_panels(&mut self) {
        let Self { left, right, .. } = self;
        left.swap_contents(right);
        // The active side has not changed, but what it is showing has.
        //
        self.sync_active_cwd();
    }

    // ---------------------------------------------------------- backends ----

    /// The backend that services `path`.
    ///
    /// Local for a local path, and the open [`crate::vfs::ArchiveFs`] for a
    /// path inside an archive - whose [`Vfs::capabilities`] are the honest,
    /// per-format ones (`.rar` is read-only; a streamed member is not
    /// seekable). **Blocking**: opening an archive detects its
    /// format, and a nested one is extracted to the session cache first, so
    /// call it from the event loop or a job thread and never from `dispatch`.
    pub fn vfs_for(&self, path: &VfsPath) -> Result<Arc<dyn Vfs>> {
        self.router.backend_for(path)
    }

    /// The session holding this application's open archives and its extracted
    /// temp files, creating it if this is the first archive.
    ///
    /// Fallible and blocking for the same reason [`App::vfs_for`] is: the
    /// session is a `0700` directory that has to be made.
    pub fn archive_session(&self) -> Result<Arc<ArchiveSession>> {
        self.router.session().map(Arc::clone)
    }

    /// The archive session **only if one has already been created**.
    ///
    /// Never creates one, so it costs nothing and touches nothing; this is the
    /// question a status line or a shutdown path asks.
    pub fn open_archive_session(&self) -> Option<&Arc<ArchiveSession>> {
        self.router.open_session()
    }
}

/// The backend to open a file through when it is being treated as a container.
///
/// [`container_kind`]'s answer where it has one, and [`BackendKind::Archive`]
/// where it does not - because by the time this is asked, something has
/// already decided the file *is* a container, and an archive is the reading
/// that a content sniff can still overturn.
///
/// Split out so `Alt+F6` and `Enter` cannot disagree about what an `.iso` is:
/// unpack used to hardcode the archive backend and so failed on every disk
/// image with a message about archives.
#[must_use]
pub fn container_backend(name: &str) -> BackendKind {
    container_kind(name).unwrap_or(BackendKind::Archive)
}

/// Which kind of container a file name claims to be, if any.
///
/// The name is a hint and never the answer: the design sniffs an archive's
/// content and the design says the same of a disk image, and neither can
/// be done here because `dispatch` may not read.
/// What this decides is only which segment is worth pushing optimistically.
///
/// The archive question is asked first, so the archive kind wins when both
/// answer yes: an archive name is the specific claim and `.img` is the vague
/// one. Nothing in the table ends in `.iso` or `.img`, so the two
/// are disjoint as things stand and the order decides nothing today. It is
/// written down because a tenth format is a line in a table and the precedence
/// it would need is not.
fn container_kind(name: &str) -> Option<BackendKind> {
    if FormatId::from_name(name).is_some() {
        return Some(BackendKind::Archive);
    }
    if crate::vfs::image::format::looks_like_image_name(name) {
        return Some(BackendKind::Image);
    }
    None
}

/// The name to put the cursor on when a path is left.
///
/// Normally the innermost component - `/a/b` leaves `b` behind in `/a`. The
/// case that needs saying is the **root of an archive**: `/a/b.zip#/` has no
/// innermost component of its own, and what the eye is looking for after
/// leaving it is `b.zip` in `/a`. Without this, `..` and `Backspace` out of an
/// archive would land at the top of the parent listing rather than back on the
/// archive that was just being browsed.
pub fn leaving_name(path: &VfsPath) -> Option<String> {
    if let Some(name) = path.file_name() {
        return Some(name);
    }
    // No name of its own: walk out through the segments until one has one.
    path.segments()
        .iter()
        .rev()
        .find_map(|(_, p)| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests;
