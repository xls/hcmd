//! Connecting to another machine, and the questions a connection asks.
//!
//!
//! **At most one connect attempt is live.** Every event an attempt produces
//! carries the [`ConnectId`] it answers to, and an event from an attempt that
//! has been abandoned is dropped without reaching the screen: starting a
//! second connection cannot be undone by the first one finishing late.
//!
//! Every question the attempt asks holds its own reply channel, and dropping
//! that channel is a refusal. Cancelling is therefore the **absence** of an
//! answer rather than an extra code path, which is what makes `Esc` on a host
//! key prompt mean the same thing as closing the dialog any other way.
//!
//! A password is never written down. The saved hosts hold what is needed to
//! find a machine again and nothing that would get into it.

use std::sync::Arc;

use crate::app::{App, ConnectRequest, RemoteEvent};
use crate::input::DialogId;
use crate::ops::JobId;
use crate::panel::{RemoteView, Side, Tab};
use crate::remote::auth::AuthPlan;
use crate::remote::connect::ConnectId;
use crate::remote::hosts::SavedHost;
use crate::remote::{RemoteId, RemoteRegistry, Target};
use crate::vfs::VfsPath;

impl App {
    /// The registry, for the panel renderer and the quit prompt.
    pub fn remotes(&self) -> &Arc<RemoteRegistry> {
        self.router.remotes()
    }

    /// The backend one connection is, when it is still open.
    pub fn remote_fs(&self, id: RemoteId) -> Option<Arc<crate::remote::RemoteFs>> {
        self.router.remotes().get(id)
    }

    /// `Ctrl+F`: the connect dialog on a local panel, the
    /// disconnect prompt on a connected one.
    ///
    /// The whole of the toggle, in one place, so the key has exactly one
    /// meaning per panel state and neither half can drift from the other.
    pub fn connect_toggle(&mut self) {
        let side = self.active_side;
        let index = self.panel(side).active_index();
        if let Some(view) = self.panel(side).tab(index).and_then(Tab::remote_view) {
            let authority = view.authority.clone();
            let id = view.id;
            let mut lines = vec![format!("Disconnect from {authority}?")];
            // the design asks the prompt to say when a job is running on
            // the connection, because the answer changes what disconnecting
            // costs.
            if self.job_on(id).is_some() {
                lines.push(String::new());
                lines.push(
                    "An operation is still running on this connection and will stop.".to_string(),
                );
            }
            self.push_dialog(Box::new(
                crate::dialog::ConfirmDialog::new(DialogId::ConfirmDisconnect, "Disconnect", lines)
                    // Pressing the key again is the answer, so `Enter` takes it:
                    // disconnecting is what `Ctrl+F` on a connected panel is for,
                    // and it is reversible in a keystroke.
                    .defaulting_to_yes(),
            ));
            return;
        }
        self.open_connect_dialog(String::new());
    }

    /// the connect dialog, with the quick-connect line seeded.
    ///
    /// The host book was read at startup by [`App::load_hosts`], because a
    /// dialog may not touch the filesystem.
    pub fn open_connect_dialog(&mut self, line: String) {
        let user = std::env::var("USER").unwrap_or_else(|_| "root".to_string());
        let default = crate::remote::Protocol::parse(&self.config.remote.default_protocol)
            .unwrap_or_default();
        let keyring = crate::remote::keyring::store().available();
        let dialog = crate::remote::connect::ConnectDialog::new(
            self.hosts.hosts().to_vec(),
            default,
            user,
            keyring,
        )
        .with_quick_search(
            self.config.panel.quick_search,
            self.config.panel.quick_search_case,
        )
        .with_line(line);
        self.push_dialog(Box::new(dialog));
    }

    /// Queue the connect dialog's answer.
    ///
    /// Queued rather than performed, because connecting is I/O and `dispatch`
    /// may not do any.
    pub fn request_connect(&mut self, request: ConnectRequest) {
        self.connector.queue(request);
    }

    /// The next attempt id. Monotonic, so an answer to an abandoned attempt is
    /// dropped rather than applied.
    pub fn next_connect_id(&mut self) -> ConnectId {
        self.connector.next_id()
    }

    /// Build the request `Ctrl+F`'s answer describes, on the active panel.
    ///
    /// One place, so a connect from the dialog and a reconnect from `Ctrl+R`
    /// cannot disagree about what the tab remembers.
    pub fn connect_answered(&mut self, answer: Box<crate::dialog::ConnectAnswer>) {
        if let Some(hosts) = answer.hosts.clone() {
            self.hosts.replace(hosts);
        }
        let side = self.active_side;
        let tab = self.panel(side).active_index();
        let origin = self.panel(side).active_tab().path.clone();
        let origin_cursor = self
            .panel(side)
            .active_tab()
            .current()
            .filter(|e| !e.is_parent)
            .map(|e| e.name.clone());
        // The other panel's initial local directory, applied
        // now rather than on success: it is a local navigation and it is what
        // the user asked the host book for.
        if let Some(dir) = answer.local_dir.clone() {
            let other = side.other();
            self.navigate(other, VfsPath::local(dir));
        }
        let attempt = self.next_connect_id();
        self.message = Some(format!("Connecting to {}...", answer.target.authority()));
        self.request_connect(ConnectRequest {
            answer,
            side,
            tab,
            origin,
            origin_cursor,
            attempt,
            reconnect: None,
        });
    }

    /// The connection the event loop should start, and what to remember about
    /// it while it runs.
    pub fn take_pending_connect(&mut self) -> Option<Box<ConnectRequest>> {
        self.connector.start()
    }

    /// A [`RemoteEvent`] from a connect task.
    ///
    /// An event from an attempt the user has already abandoned is **dropped**,
    /// which drops its reply channel with it - and a dropped reply is a
    /// refusal, so an abandoned host-key question can never be answered `yes`
    /// by accident (the design, S6).
    pub fn apply_remote_event(&mut self, event: RemoteEvent) {
        match event {
            RemoteEvent::HostKey {
                attempt,
                target,
                fingerprint,
                reply,
            } => {
                if !self.connector.is_live(attempt) {
                    return;
                }
                let lines = crate::remote::known_hosts::unknown_lines(&target, &fingerprint);
                self.connector.hold_host_key(reply);
                // `Cancel` is the default button: the delete confirmation
                // defaults to its affirmative because the user has already
                // chosen, and here they have not.
                self.push_dialog(Box::new(
                    crate::dialog::ConfirmDialog::new(DialogId::HostKey, "Unknown host key", lines)
                        .with_buttons("Accept", "Cancel"),
                ));
            }
            RemoteEvent::HostKeyChanged {
                attempt,
                target,
                fingerprint,
                line,
                file,
            } => {
                if !self.connector.is_live(attempt) {
                    return;
                }
                let verdict = crate::remote::known_hosts::Verdict::Changed { line, fingerprint };
                let lines = crate::remote::known_hosts::changed_lines(&target, &verdict, &file);
                // A message, not a question: one button, no affirmative,
                // nothing to accept. The connection is already aborted by the
                // time this is drawn (S6).
                self.push_dialog(Box::new(
                    crate::dialog::MessageDialog::new("Host key changed", lines)
                        .with_id(DialogId::HostKeyChanged),
                ));
            }
            RemoteEvent::Secret {
                attempt,
                kind,
                offer_keyring,
                reply,
            } => {
                if !self.connector.is_live(attempt) {
                    return;
                }
                let mut dialog = crate::remote::prompt::SecretDialog::new(kind, offer_keyring);
                // with no keyring, "say so in the dialog and
                // fall back to prompting every time".
                if !crate::remote::keyring::store().available() {
                    dialog = dialog.with_note(crate::remote::keyring::unavailable_message());
                }
                self.connector.hold_secret(reply);
                self.push_dialog(Box::new(dialog));
            }
            RemoteEvent::Connected {
                attempt,
                id,
                start,
                saved,
            } => {
                if let Some(target) = saved {
                    self.remember_keyring_host(&target);
                }
                if !self.connector.is_live(attempt) {
                    // The tab has moved on; the connection would otherwise be
                    // an orphan holding a socket open.
                    self.router.remotes().close(id);
                    return;
                }
                self.finish_connect(id, start);
            }
            RemoteEvent::Failed { attempt, message } => {
                if !self.connector.is_live(attempt) {
                    return;
                }
                self.connector.abandon();
                self.message = Some(message);
            }
        }
    }

    /// Point the tab at a connection that has just come up.
    fn finish_connect(&mut self, id: RemoteId, start: VfsPath) {
        let Some(live) = self.connector.finish() else {
            self.router.remotes().close(id);
            return;
        };
        let authority = live.target.authority();
        let (side, index) = (live.side, live.tab);
        let Some(tab) = self.panel_mut(side).tab_mut(index) else {
            self.router.remotes().close(id);
            return;
        };
        // A reconnect keeps the view it already had, including where the tab
        // came from: it is the same tab on the same host.
        let view = match tab.remote_view.take() {
            Some(mut existing) => {
                existing.disconnected = false;
                existing.authority = authority.clone();
                existing.id = id;
                existing
            }
            None => Box::new(RemoteView {
                id,
                authority: authority.clone(),
                origin: live.origin.clone(),
                origin_cursor: live.origin_cursor.clone(),
                disconnected: false,
            }),
        };
        tab.remote_view = Some(view);
        tab.path = start.clone();
        tab.title = start.display_title();
        tab.marks.clear();
        // "the active panel becomes the remote listing in place". In place,
        // not on top of: the batch arm only clears when it is told to
        // (`Tab::replace_on_next_batch`), so without this the first remote
        // batch is *appended* to the local rows the tab was showing - two `..`
        // rows, the union of two directories, and a status line counting both.
        // `App::reread` and `App::disconnect` each clear their own way; a
        // fresh connection has nothing worth keeping on screen, so it clears
        // now rather than on arrival, and the cursor goes with the rows it was
        // indexing. A reconnect takes this path too, where the stale listing
        // is the disconnected one.
        tab.clear_entries();
        tab.cursor = 0;
        tab.scroll = 0;
        // The protocol is known the moment the connection is up, so the tab
        // gets the real answer here rather than `REMOTE_UNKNOWN` until the
        // first listing lands - which is what refused `F6` on a freshly
        // connected SFTP panel while its own rows were on screen. This does
        // not block: a remote backend fixes its capabilities when the
        // connection is established, so resolving one is a registry lookup and
        // a field read (the design I5).
        self.router.resolve_capabilities(&start);
        self.refresh_caps(side, index);
        self.request_read(side, index, start);
        self.message = Some(format!("Connected to {authority}"));
    }

    /// `Ctrl+F` on a connected panel: close the connection and
    /// return the tab to the local directory it remembered.
    ///
    /// Never touches the other panel (the design I9).
    pub fn disconnect(&mut self, side: Side, tab: usize) {
        let Some(view) = self
            .panel_mut(side)
            .tab_mut(tab)
            .and_then(|t| t.remote_view.take())
        else {
            return;
        };
        self.router.remotes().close(view.id);
        // A remembered answer must not outlive the backend it describes: this
        // connection is gone, and every path on it is now serviced by nothing.
        // Forgetting the subtree is what makes the next gate on such a path
        // conservative rather than confidently wrong.
        self.router
            .capability_cache()
            .forget_subtree(&view.id.root());
        if let Some(t) = self.panel_mut(side).tab_mut(tab) {
            t.marks.clear();
            t.pending_select = view.origin_cursor.clone();
        }
        // The active tab is the one `Ctrl+F` was pressed on; navigating any
        // other tab is not something this program does, and `navigate` acts on
        // the active one.
        if self.panel(side).active_index() == tab {
            self.navigate_selecting(side, view.origin.clone(), view.origin_cursor.clone());
        } else if let Some(t) = self.panel_mut(side).tab_mut(tab) {
            t.path = view.origin.clone();
            t.title = view.origin.display_title();
            t.clear_entries();
            t.pending_select = view.origin_cursor.clone();
            let path = view.origin.clone();
            // The active tab takes the `navigate_selecting` branch above,
            // which refreshes on its way past; an inactive one is repointed by
            // hand here and has to be refreshed by hand with it, or it would
            // keep describing the connection it has just left.
            self.refresh_caps(side, tab);
            self.request_read(side, tab, path);
        }
        self.message = Some(format!("Disconnected from {}", view.authority));
    }

    /// Whether a job is running against this connection, which is what the
    /// disconnect prompt has to say.
    pub fn job_on(&self, id: RemoteId) -> Option<JobId> {
        self.jobs
            .rows()
            .iter()
            .filter(|status| status.finished.is_none())
            .find(|status| {
                self.jobs.spec(status.id).is_some_and(|spec| {
                    spec.sources
                        .iter()
                        .chain(spec.dest.iter())
                        .chain(spec.targets.iter())
                        .any(|path| RemoteId::from_path(path) == Some(id))
                })
            })
            .map(|status| status.id)
    }

    /// Mark every tab on a lost connection as disconnected.
    ///
    /// Called once a frame from the event loop: the last listing stays on
    /// screen, greyed, and the path is not lost.
    pub fn service_remote_liveness(&mut self) {
        let lost: Vec<RemoteId> = self
            .router
            .remotes()
            .ids()
            .into_iter()
            .filter(|id| {
                self.router
                    .remotes()
                    .get(*id)
                    .is_some_and(|fs| fs.is_lost())
            })
            .collect();
        if lost.is_empty() {
            return;
        }
        for side in [Side::Left, Side::Right] {
            let count = self.panel(side).tabs().len();
            for index in 0..count {
                let Some(tab) = self.panel_mut(side).tab_mut(index) else {
                    continue;
                };
                let Some(view) = tab.remote_view.as_deref_mut() else {
                    continue;
                };
                if lost.contains(&view.id) {
                    view.disconnected = true;
                }
            }
        }
    }

    /// `Ctrl+R` on a disconnected tab reconnects it.
    ///
    /// `true` when it took the key. It re-runs the whole connect sequence for
    /// the tab's own target, including host-key verification and
    /// authentication; a password typed for the previous connection is **not**
    /// kept, because it was held for the session of that connection.
    pub fn reconnect(&mut self, side: Side) -> bool {
        let index = self.panel(side).active_index();
        let Some(view) = self.panel(side).tab(index).and_then(Tab::remote_view) else {
            return false;
        };
        if !view.disconnected {
            return false;
        }
        let id = view.id;
        let origin = view.origin.clone();
        let origin_cursor = view.origin_cursor.clone();
        let Some(fs) = self.router.remotes().get(id) else {
            self.message = Some("that connection has been closed; press Ctrl+F".to_string());
            return true;
        };
        let target = fs.target().clone();
        let home =
            crate::config::paths::home_dir().unwrap_or_else(|_| std::path::PathBuf::from("/"));
        let saved = self
            .hosts
            .hosts()
            .iter()
            .find(|h| h.target() == target)
            .cloned();
        let plan = match (&saved, target.protocol.verifies_host_key()) {
            (Some(host), true) => AuthPlan::for_host(host, &home),
            (host, false) => AuthPlan::for_password_login(host.as_ref()),
            (None, true) => AuthPlan::for_host(&SavedHost::default(), &home),
        };
        let attempt = self.next_connect_id();
        self.message = Some(format!("Reconnecting to {}...", target.authority()));
        let dir = self
            .panel(side)
            .tab(index)
            .map(|t| t.path.tail().to_string_lossy().into_owned());
        self.request_connect(ConnectRequest {
            answer: Box::new(crate::dialog::ConnectAnswer {
                target: Target { dir, ..target },
                plan,
                password: None,
                local_dir: None,
                hosts: None,
            }),
            side,
            tab: index,
            origin,
            origin_cursor,
            attempt,
            reconnect: Some(id),
        });
        true
    }

    /// Record that a target's password now lives in the keyring.
    ///
    ///
    /// The half of the opt-in that is not the secret. `Method::Stored` is in
    /// an auth plan only for a saved host whose `auth` is already `keyring`,
    /// so without this the tick stored a password nothing would ever read
    /// back and the next connect prompted exactly as if nothing had been
    /// ticked. A quick connect has no saved host to opt in on, so one is
    /// created from the target just reached: there was nothing to tick before,
    /// and refusing to save because no row existed yet answers a question the
    /// user did not ask.
    ///
    /// No secret is written here. `hosts.toml` never holds one, and this
    /// only sets `auth`.
    pub fn remember_keyring_host(&mut self, target: &crate::remote::Target) {
        use crate::remote::hosts::AuthMethod;
        let existing = self.hosts.hosts_mut().iter_mut().find(|h| {
            h.protocol == target.protocol
                && h.host == target.host
                && h.port == target.port
                && h.username == target.user
        });
        match existing {
            Some(host) => {
                if host.auth == AuthMethod::Keyring {
                    return;
                }
                host.auth = AuthMethod::Keyring;
            }
            None => self
                .hosts
                .hosts_mut()
                .push(crate::remote::hosts::SavedHost {
                    label: target.authority(),
                    protocol: target.protocol,
                    host: target.host.clone(),
                    port: target.port,
                    username: target.user.clone(),
                    auth: AuthMethod::Keyring,
                    ..crate::remote::hosts::SavedHost::default()
                }),
        }
    }

    /// The unknown-host prompt was answered.
    ///
    /// `false` by any route - `Cancel`, `Esc`, a closed dialog - refuses, and
    /// so does an answer that arrives with nothing waiting for it.
    pub fn answer_host_key(&mut self, accepted: bool) {
        self.connector.answer_host_key(accepted);
        if !accepted {
            self.message = Some("the host key was not accepted".to_string());
        }
    }

    /// The password prompt was answered.
    pub fn answer_secret(&mut self, answer: Option<crate::dialog::SecretAnswer>) {
        self.connector.answer_secret(answer);
    }

    /// Give up on the attempt now running, dropping every question with it.
    ///
    /// Dropping a reply channel is a refusal, so this needs no cooperation
    /// from the task: it sees its question refused and stops.
    pub fn abandon_connect(&mut self) {
        self.connector.abandon();
    }

    /// The host book, read once at startup the way `load_search_state` reads
    /// `searches.toml`.
    pub fn load_hosts(&mut self) {
        let (hosts, warnings) = crate::remote::hosts::load();
        self.hosts.adopt(hosts);
        self.warnings.extend(warnings);
    }

    /// The host book as it needs writing, or `None` when the file and memory
    /// already agree. The event loop's, like [`App::take_hotlist_write`].
    ///
    /// Not written here. The write is a `create_dir_all`, a serialise and a
    /// `std::fs::write` on the config directory, and this runs on the thread
    /// that draws - which never blocks on I/O. The caller spawns it and puts
    /// what came back on the status line, because the design keeps
    /// configuration problems non-fatal and never fatal.
    ///
    /// The hosts are handed over and the book marked clean in the same act, so
    /// a change made while the write is in flight becomes the next write
    /// rather than a second copy of this one. A write that failed is reported
    /// once and not retried on every frame for the rest of the session, which
    /// is what the dirty flag meant before the write moved off this thread.
    pub fn take_hosts_write(&mut self) -> Option<Vec<crate::remote::hosts::SavedHost>> {
        if !self.hosts.is_dirty() {
            return None;
        }
        let hosts = self.hosts.hosts().to_vec();
        // `adopt` is how this type is told the file and memory agree again,
        // and handing the hosts to the writer is the moment that becomes true
        // as far as anything queued here is concerned.
        self.hosts.adopt(hosts.clone());
        Some(hosts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::VfsEvent;
    use crate::app::tests::{app_with, connect_active, nas};
    use crate::ops::JobSpec;
    use crate::vfs::Entry;

    /// "`Ctrl+F` is a toggle whose meaning depends on the active
    /// panel's state: local, the connect dialog; connected, a disconnect
    /// prompt."
    #[test]
    fn ctrl_f_is_one_key_resolved_by_panel_state() {
        let mut app = app_with(&["a"]);
        app.connect_toggle();
        assert_eq!(
            app.top_dialog().map(crate::dialog::Dialog::id),
            Some(DialogId::Connect)
        );
        app.close_dialogs();

        connect_active(&mut app);
        app.connect_toggle();
        assert_eq!(
            app.top_dialog().map(crate::dialog::Dialog::id),
            Some(DialogId::ConfirmDisconnect)
        );
    }

    /// I8 and I9: the tab is connected exactly while its path is on that
    /// connection, and disconnecting returns it to where it was without
    /// touching the other panel.
    #[test]
    fn connecting_moves_the_tab_in_place_and_disconnecting_puts_it_back() {
        let mut app = app_with(&["a"]);
        app.left.active_tab_mut().path = VfsPath::local("/home/thorin");
        app.left.active_tab_mut().cursor = 0;
        let before_right = app.right.active_tab().path.clone();

        let id = connect_active(&mut app);
        let tab = app.left.active_tab();
        assert_eq!(tab.path, id.path("/srv"));
        assert_eq!(tab.remote_view().map(|v| v.id), Some(id), "I8");
        assert_eq!(
            RemoteId::from_path(&tab.path),
            Some(id),
            "I8: the ids agree"
        );
        assert!(tab.is_remote());
        assert_eq!(
            app.right.active_tab().path,
            before_right,
            "I9: the other panel is untouched"
        );

        app.disconnect(Side::Left, 0);
        let tab = app.left.active_tab();
        assert_eq!(tab.path, VfsPath::local("/home/thorin"), "I9");
        assert!(!tab.is_remote(), "I8 holds in both directions");
        assert!(
            app.remotes().get(id).is_none(),
            "disconnecting closed the connection"
        );
        assert_eq!(app.right.active_tab().path, before_right);
    }

    /// "the active panel becomes the remote listing in place".
    ///
    /// In place, and not merged with what was there: the first remote batch
    /// used to be appended to the local rows the tab was still showing, which
    /// gave the panel two `..` rows, the union of two directories and a status
    /// line counting both. `Ctrl+R` corrected it, which is what made it look
    /// like a redraw problem rather than a missing clear.
    #[test]
    fn connecting_replaces_the_local_listing_rather_than_adding_to_it() {
        let mut app = app_with(&["LOCALDIR", "LOCAL_ONLY_A.txt"]);
        let mut parent = Entry::dir("..");
        parent.is_parent = true;
        app.left.active_tab_mut().entries.insert(0, parent);
        app.left.active_tab_mut().cursor = 2;

        let id = connect_active(&mut app);
        assert!(
            app.left.active_tab().entries.is_empty(),
            "the local rows went with the local directory"
        );
        assert_eq!(app.left.active_tab().cursor, 0, "and so did the cursor");

        // The listing the connection produces, delivered on the generation
        // `finish_connect` queued - the one `main`'s reader would answer.
        let generation = app.left.active_tab().generation;
        let mut remote_parent = Entry::dir("..");
        remote_parent.is_parent = true;
        app.apply_vfs_event(VfsEvent::Entries {
            side: Side::Left,
            tab: 0,
            generation,
            batch: vec![
                remote_parent,
                Entry::dir("nested"),
                Entry::file("alpha.txt"),
            ],
        });

        let names: Vec<&str> = app
            .left
            .active_tab()
            .entries
            .iter()
            .map(|e| e.name.as_str())
            .collect();
        assert_eq!(names, ["..", "nested", "alpha.txt"], "{names:?}");
        assert_eq!(
            names.iter().filter(|n| **n == "..").count(),
            1,
            "one parent row, which every panel invariant depends on"
        );
        assert_eq!(app.left.active_tab().path, id.path("/srv"));
    }

    /// a connection with a job on it is not closed under the
    /// job, whichever key leaves the panel.
    ///
    /// `App::close_tab` has always refused to; `App::navigate` did not, so a
    /// `Ctrl+G` off a connected panel closed the transport under a running
    /// copy. The batch then failed once per remaining file with the identical
    /// "that connection has been closed" - the failure summary
    /// `ops::is_fatal` exists to prevent.
    #[test]
    fn navigating_off_a_connection_leaves_a_running_job_its_transport() {
        let mut app = app_with(&["a"]);
        let id = connect_active(&mut app);
        let job = app.queue_job(JobSpec::new(
            crate::ops::JobKind::Copy,
            vec![id.path("/srv/a.txt")],
            Some(VfsPath::local("/tmp")),
        ));
        assert_eq!(app.job_on(id), Some(job));

        app.navigate(Side::Left, VfsPath::local("/tmp"));
        assert!(
            !app.left.active_tab().is_remote(),
            "I8: the tab left the connection"
        );
        assert!(
            app.remotes().get(id).is_some(),
            "the job is still using it, so it is still open"
        );
        let said = app.message.clone().unwrap_or_default();
        assert!(said.contains("stays connected"), "{said}");

        // With no job on it, the same key closes it, which is the rule that
        // keeps a connection from outliving the panel that opened it.
        let mut app = app_with(&["a"]);
        let id = connect_active(&mut app);
        app.navigate(Side::Left, VfsPath::local("/tmp"));
        assert!(app.remotes().get(id).is_none());
    }

    /// I10: a connected tab is persisted as the local directory it remembered,
    /// never as a `Remote(3)` path that would name nothing next session.
    ///
    #[test]
    fn a_connected_tab_is_saved_as_the_directory_it_came_from() {
        let mut app = app_with(&["a"]);
        app.left.active_tab_mut().path = VfsPath::local("/home/thorin");
        connect_active(&mut app);
        let saved = crate::panel::state::snapshot(&app.left);
        assert_eq!(
            saved.tabs.first().map(|t| t.path.as_str()),
            Some("/home/thorin")
        );
    }

    /// the disconnected state: the last listing stays, greyed, and
    /// the path is not lost.
    #[test]
    fn a_dropped_connection_greys_the_panel_without_losing_the_path() {
        let mut app = app_with(&["a"]);
        let id = connect_active(&mut app);
        let path = app.left.active_tab().path.clone();

        app.remote_fs(id).expect("connected").close();
        app.service_remote_liveness();

        let tab = app.left.active_tab();
        assert!(tab.is_disconnected());
        assert_eq!(tab.path, path, "the path survives the drop");
        assert!(tab.is_remote(), "and the tab is still a remote tab");
    }

    /// S6: a changed host key is a **message**, not a question, and there is no
    /// code path from it to a connection.
    #[test]
    fn a_changed_host_key_is_a_message_with_nothing_to_accept() {
        let mut app = app_with(&["a"]);
        app.connect_answered(Box::new(crate::dialog::ConnectAnswer {
            target: nas(),
            plan: AuthPlan::for_password_login(None),
            password: None,
            local_dir: None,
            hosts: None,
        }));
        let request = app.take_pending_connect().expect("queued");
        app.apply_remote_event(RemoteEvent::HostKeyChanged {
            attempt: request.attempt,
            target: nas(),
            fingerprint: "SHA256:abcdef".to_string(),
            line: 12,
            file: std::path::PathBuf::from("/home/thorin/.ssh/known_hosts"),
        });
        let dialog = app.top_dialog().expect("a dialog");
        assert_eq!(dialog.id(), DialogId::HostKeyChanged);
        assert!(
            dialog.mnemonic_letters().is_empty(),
            "one button, no affirmative"
        );
        assert!(!app.left.active_tab().is_remote(), "nothing was connected");
    }

    /// An answer to an attempt the user has abandoned is dropped, and dropping
    /// the reply channel is a refusal.
    #[test]
    fn a_host_key_question_from_an_abandoned_attempt_is_refused() {
        let mut app = app_with(&["a"]);
        app.connect_answered(Box::new(crate::dialog::ConnectAnswer {
            target: nas(),
            plan: AuthPlan::for_password_login(None),
            password: None,
            local_dir: None,
            hosts: None,
        }));
        let live = app.take_pending_connect().expect("queued");
        let stale = ConnectId(live.attempt.0.saturating_sub(1));

        let (reply, answer) = tokio::sync::oneshot::channel();
        app.apply_remote_event(RemoteEvent::HostKey {
            attempt: stale,
            target: nas(),
            fingerprint: "SHA256:abcdef".to_string(),
            reply,
        });
        assert!(!app.dialog_is_open(), "no dialog for an abandoned attempt");
        assert!(answer.blocking_recv().is_err(), "the reply channel dropped");

        // And the live attempt's question, answered `Cancel`, refuses.
        let (reply, answer) = tokio::sync::oneshot::channel();
        app.apply_remote_event(RemoteEvent::HostKey {
            attempt: live.attempt,
            target: nas(),
            fingerprint: "SHA256:abcdef".to_string(),
            reply,
        });
        assert_eq!(
            app.top_dialog().map(crate::dialog::Dialog::id),
            Some(DialogId::HostKey)
        );
        app.answer_host_key(false);
        assert_eq!(answer.blocking_recv(), Ok(false), "never a default accept");
    }

    /// I16: `F8` on a remote selection is the permanent-delete confirmation,
    /// never the trash one - there is no trash on a filesystem that is not this
    /// machine's.
    #[test]
    fn a_remote_path_has_nowhere_to_be_trashed_to() {
        let split = crate::ops::delete::split_by_trash(&[RemoteId(3).path("/srv/a.txt")]);
        assert!(split.trashable.is_empty());
        assert_eq!(split.untrashable.len(), 1, "I16");
    }

    /// S8: a password never reaches a file under `~/.config`.
    ///
    #[test]
    fn a_password_never_reaches_the_host_book() {
        let secret = crate::remote::secret::Secret::from_str("hunter2");
        let answer = crate::dialog::ConnectAnswer {
            target: nas(),
            plan: AuthPlan::for_password_login(None),
            password: Some(secret),
            local_dir: None,
            hosts: Some(vec![crate::remote::hosts::SavedHost {
                label: "nas".to_string(),
                protocol: crate::remote::Protocol::Sftp,
                host: "nas.local".to_string(),
                port: 2222,
                username: "thorin".to_string(),
                ..crate::remote::hosts::SavedHost::default()
            }]),
        };
        let mut app = app_with(&["a"]);
        app.connect_answered(Box::new(answer));
        assert!(app.hosts.is_dirty(), "the book was edited");
        let rendered = crate::remote::hosts::render(app.hosts.hosts()).expect("render");
        assert!(!rendered.contains("hunter2"), "S8: {rendered}");
        assert!(!rendered.contains("password"), "there is no password field");
        // And nothing the request carries prints one either (S1, S3).
        let request = app.take_pending_connect().expect("queued");
        assert!(!format!("{:?}", request.answer.password).contains("hunter2"));
    }
}
