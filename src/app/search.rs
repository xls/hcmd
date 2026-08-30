//! Searching, and the virtual listing a search puts in a panel.
//!
//!
//! A search is **queued** for the event loop and never performed by
//! [`crate::input::dispatch`]: compiling a query and spawning a walk are both
//! work `dispatch` may not do. A remote content search is queued only after
//! the user has agreed to it, because it costs somebody else's machine.
//!
//! A tab shows at most one virtual listing. Searching again from a listing
//! replaces it and keeps the **first** origin, because a `list:` path is not a
//! directory to come back to. Leaving one cancels the walk that was filling
//! it, and cancelling is the forgetting: there is one cancellation flag and it
//! is the listing's own, so no caller has to remember to stop the walk
//! separately.
//!
//! `Ctrl+W` lives here rather than on [`crate::panel::Panel`] because a panel
//! has no route to the listing registry, and a closed tab is one of the four
//! ways a listing ends.

use std::sync::Arc;

use crate::app::{App, PendingView, ReadRequest, SearchRequest, StartedSearch};
use crate::input::DialogId;
use crate::panel::{Side, Tab, VirtualKind, VirtualView};
use crate::remote::RemoteId;
use crate::search::Query;
use crate::search::walk::Tally;
use crate::vfs::ListFs;

impl App {
    /// Queue a search into a panel.
    ///
    /// Queued, never performed: `dispatch` may not touch the filesystem
    /// and starting a search registers a listing and
    /// spawns a walk. The results go into **the panel that was active when the
    /// search started**, which is decided here rather than one frame later.
    pub fn request_search(&mut self, mut query: Query, kind: VirtualKind) {
        // the two switches that are configuration rather than
        // dialog controls, stamped here so every route into a search obeys
        // them: `Alt+F7`, `Ctrl+B` and `Alt+Shift+F7` all come through this
        // one function, and neither the dialog nor `Query::branch` is handed
        // a `Config` to read them from.
        query.respect_gitignore = self.config.search.respect_gitignore;
        // No new configuration key: a search that followed
        // links would hang on the first loop and `ignore` does not detect one,
        // so it is `ops.follow_symlinks` or nothing.
        query.follow_symlinks = self.config.ops.follow_symlinks;
        // a content search is a search, so its pattern becomes
        // the session's and the next viewer opens with it loaded. A name-only
        // search sets nothing - the mask is a different language from the find
        // bar's and installing it would put a glob in a text search.
        if let Some(content) = query.content.as_ref() {
            self.viewers.last_find = Some(crate::viewer::find::FindQuery {
                input: content.pattern.clone(),
                kind: match content.mode {
                    crate::search::query::TextMode::Hex => crate::viewer::find::FindKind::Hex,
                    // The viewer's find bar cannot compile a regex yet
                    // (`viewer::find::REGEX_MILESTONE`), so a regex pattern
                    // travels as the text it is rather than as nothing: it
                    // still finds itself in the common case, and the bar says
                    // what it is doing if it does not.
                    crate::search::query::TextMode::Plain
                    | crate::search::query::TextMode::Regex => crate::viewer::find::FindKind::Text,
                },
                case: if content.case_sensitive {
                    crate::config::QuickSearchCase::Sensitive
                } else {
                    crate::config::QuickSearchCase::Insensitive
                },
            });
        }
        let side = self.active_side;
        let tab = self.panel(side).active_index();
        let request = Box::new(SearchRequest {
            side,
            tab,
            query,
            kind,
        });
        // "Remote content search is opt-in and warns about the
        // transfer cost." A name-only search of a remote root runs with no
        // prompt - it reads listings, which is what browsing already does.
        let hosts = self.remote_hosts_in(&request);
        if request.query.content.is_some() && !hosts.is_empty() {
            self.hold_remote_search(request, hosts);
            return;
        }
        self.search.pending = Some(request);
    }

    /// The authorities of every remote root a search would read from.
    /// Empty for a search that never leaves this machine.
    fn remote_hosts_in(&self, request: &SearchRequest) -> Vec<String> {
        let mut hosts: Vec<String> = Vec::new();
        for root in &request.query.roots {
            let Some(id) = RemoteId::from_path(root) else {
                continue;
            };
            let name = self
                .router
                .remotes()
                .authority(id)
                .unwrap_or_else(|| id.to_string());
            if !hosts.contains(&name) {
                hosts.push(name);
            }
        }
        hosts
    }

    /// Drain the queued search. The event loop calls this once a frame.
    pub fn take_pending_search(&mut self) -> Option<Box<SearchRequest>> {
        self.search.pending.take()
    }

    /// Register the listing, point the tab at it, and spawn the walk.
    ///
    ///
    /// **Event loop only**: it compiles, registers, re-points a tab and spawns
    /// blocking work.
    ///
    /// Everything after this is the ordinary directory-read path. The tab's
    /// path becomes a `list:` path, the registered [`ListFs`] is a [`Vfs`], and
    /// `main::spawn_read` streams it into the panel exactly as it streams a
    /// directory - which is what the "results stream back over a
    /// channel, with a live count" means in this program: the same channel.
    pub fn start_search(&mut self, request: SearchRequest) -> Option<StartedSearch> {
        let SearchRequest {
            side,
            tab: tab_index,
            query,
            kind,
        } = request;
        // the design offers `search.engine = "external"` and the design
        // rules out the subprocess it would need. Said once, before the walk,
        // and then the internal engine runs anyway: silently running internal
        // while the file says external is the one outcome worse than either,
        // and refusing outright would leave that user unable to search at all.
        //
        if let Some(why) = self.config.search.engine_refusal() {
            self.message = Some(why.to_string());
        }
        // The one place a pattern is refused, so the dialog's message and the
        // engine's cannot differ. Nothing is registered or spawned when it
        // fails.
        let compiled = match query.compile() {
            Ok(compiled) => Arc::new(compiled),
            Err(err) => {
                self.message = Some(err.to_string());
                return None;
            }
        };
        // Where this tab came back from. A search *from* a search keeps the
        // first one's origin: the design returns the panel to "its
        // underlying real directory", and a `list:` path is not one.
        let tab_ref = self.panel(side).tab(tab_index)?;
        let (origin, origin_cursor, previous) = match tab_ref.virtual_view() {
            Some(view) => (
                view.origin.clone(),
                view.origin_cursor.clone(),
                Some(view.listing),
            ),
            None => (tab_ref.path.clone(), tab_ref.cursor_name(), None),
        };

        let header = query.header();
        let (listing, sink) = ListFs::streaming(header.clone(), &query.roots);
        let pending = PendingView {
            kind,
            header,
            find: compiled.viewer_find(),
            origin,
            origin_cursor,
            previous,
        };
        if !self.show_listing(side, tab_index, listing, pending) {
            return None;
        }
        let listing = self
            .panel(side)
            .tab(tab_index)
            .and_then(Tab::virtual_view)
            .map(|view| view.listing)?;
        let walk = crate::search::spawn(
            Arc::clone(&self.vfs),
            compiled,
            crate::search::SearchOptions::default(),
            sink,
        );
        Some(StartedSearch {
            side,
            listing,
            walk,
        })
    }

    /// Say once what the finished walk passed over.
    ///
    /// Called by the event loop when the walk's task ends, never by
    /// `dispatch`. Silent when the walk had nothing to report, when the panel
    /// has moved on to a different listing, and after an `Esc`: a cancelled
    /// walk has already said `search stopped; 128 kept`, and a count of what a
    /// stopped walk did not reach is not news.
    pub fn report_search_tally(&mut self, started: &StartedSearch, tally: &Tally) {
        let index = self.panel(started.side).active_index();
        let showing = self
            .panel(started.side)
            .tab(index)
            .and_then(Tab::virtual_view)
            .is_some_and(|view| view.listing == started.listing);
        if !showing {
            return;
        }
        if self
            .router
            .listing(started.listing)
            .is_some_and(|listing| listing.is_cancelled())
        {
            return;
        }
        if let Some(note) = tally.note(self.config.ui.ascii_borders) {
            self.message = Some(note);
        }
    }

    /// Write back whatever the Find dialog changed.
    ///
    /// **Event loop only**, once a frame, and a no-op when nothing is dirty.
    /// A failure is a status-line message and never anything worse: a saved
    /// search that could not be written must not take the search with it
    /// (the design - a configuration problem is never fatal).
    pub fn service_search_state(&mut self) {
        if self.search.saved_dirty {
            self.search.saved_dirty = false;
            if let Err(err) = crate::search::saved::store_saved(&self.search.saved) {
                self.message = Some(err.to_string());
            }
        }
        if self.search.history_dirty {
            self.search.history_dirty = false;
            // Said, not swallowed. The one failure that matters here is a
            // refusal to overwrite a `search-history.toml` that did not parse:
            // the history came up empty *because* of that file, and a user who
            // is not told will never repair it. An ordinary write failure gets
            // the same line, which is one status message rather than a lost
            // file.
            if let Err(err) = crate::search::saved::store_history(&self.search.history) {
                self.message = Some(err.to_string());
            }
        }
    }

    /// Read `searches.toml` and `search-history.toml` into
    /// [`App::search_state`].
    ///
    /// **Event loop only**, once at startup: the Find dialog is opened from
    /// `dispatch`, which may not read the disk, so what it offers has to be
    /// in memory before the first `Alt+F7`. A file that will not parse
    /// loads as an empty list with a warning and never fails anything (a
    /// configuration problem is never fatal).
    pub fn load_search_state(&mut self) {
        let (saved, warnings) = crate::search::saved::load_saved();
        self.search.saved = saved;
        let (history, history_warnings) = crate::search::saved::load_history();
        self.search.history = history;
        self.warnings.extend(warnings);
        self.warnings.extend(history_warnings);
    }

    /// Register a listing and point a tab at it.
    ///
    /// The half of [`App::start_search`] that has nothing to do with
    /// searching: everything from here on is the ordinary directory-read path,
    /// and separating it is what lets the state machine of entering and
    /// leaving a virtual listing be tested without an engine behind it.
    ///
    /// `false` when the listing could not be registered or the tab has gone,
    /// in which case nothing has been changed and nothing should be spawned.
    pub(crate) fn show_listing(
        &mut self,
        side: Side,
        tab_index: usize,
        listing: Arc<ListFs>,
        view: PendingView,
    ) -> bool {
        let PendingView {
            kind,
            header,
            find,
            origin,
            origin_cursor,
            previous,
        } = view;
        let id = match self.router.register_listing(listing) {
            Ok(id) => id,
            Err(err) => {
                self.message = Some(err.to_string());
                return false;
            }
        };
        // A second search into the same tab replaces the first and stops it -
        // registered before forgetting, so a failure above leaves the panel
        // showing what it was showing.
        if let Some(old) = previous {
            self.router.forget_listing(old);
        }

        let path = id.to_path();
        // Read through to the one cache, which `register_listing` above has
        // just filled for this listing. Not a placeholder to be upgraded when
        // the walk finishes: a search over local roots is writable from its
        // first frame, and `F6` on a hit that has already arrived is the whole
        // point of streaming the results in.
        let caps = self.router.known_capabilities(&path);
        let generation = self.next_generation();
        let panel = self.panel_mut(side);
        panel.quick.clear();
        let Some(tab) = panel.tab_mut(tab_index) else {
            self.router.forget_listing(id);
            return false;
        };
        // The tab-bar label is the *kind*, not the header: a bar nine tabs
        // wide has three cells per label, and a header cropped to three cells
        // says nothing the word `search` does not.
        tab.title = kind.id().to_string();
        tab.path = path.clone();
        tab.entries.clear();
        tab.marks.clear();
        tab.cursor = 0;
        tab.scroll = 0;
        tab.loading = true;
        tab.generation = generation;
        tab.pending_select = None;
        tab.replace_on_next_batch = false;
        tab.caps = caps;
        tab.virtual_view = Some(Box::new(VirtualView {
            kind,
            header,
            origin,
            origin_cursor,
            listing: id,
            find,
        }));
        self.pending_reads.push(ReadRequest {
            side,
            tab: tab_index,
            generation,
            path,
        });
        true
    }

    /// `Ctrl+W`: close the active tab, forgetting any virtual listing it was
    /// showing.
    ///
    /// Here rather than on [`Panel`] because a listing outlives nothing: the
    /// panel has no route to the registry, and a closed tab is one of the four
    /// ways a listing ends. `false` when it was the last tab, which cannot be
    /// closed.
    pub fn close_tab(&mut self, side: Side) -> bool {
        let index = self.panel(side).active_index();
        let listing = self
            .panel(side)
            .tab(index)
            .and_then(Tab::virtual_view)
            .map(|view| view.listing);
        // closing a tab closes its connection, unless a job is
        // running on it - then the connection is left open and closed when the
        // job ends, and the status line says so.
        let remote = self
            .panel(side)
            .tab(index)
            .and_then(Tab::remote_view)
            .map(|view| (view.id, view.authority.clone()));
        if !self.panel_mut(side).close_tab() {
            return false;
        }
        if let Some(id) = listing {
            self.router.forget_listing(id);
        }
        if let Some((id, authority)) = remote {
            if self.job_on(id).is_some() {
                self.message = Some(format!(
                    "{authority} stays connected until the running operation finishes"
                ));
            } else {
                self.router.remotes().close(id);
            }
        }
        true
    }

    /// the opt-in: hold a content search that crosses a network
    /// until it has been asked about.
    pub fn hold_remote_search(&mut self, request: Box<SearchRequest>, hosts: Vec<String>) {
        let mut lines = vec![
            "This search reads the contents of every file the name mask admits,".to_string(),
            "and those files are on:".to_string(),
        ];
        for host in hosts {
            lines.push(format!("  {host}"));
        }
        lines.push(String::new());
        lines.push("Each one is transferred in full, once per selected charset.".to_string());
        self.search.pending_remote = Some(request);
        // `Cancel` is the default, the same shape the rewrite gate
        // has and for the same reason: the cost is invisible until it has been
        // paid.
        self.push_dialog(Box::new(crate::dialog::ConfirmDialog::new(
            DialogId::ConfirmRemoteSearch,
            "Search a remote host?",
            lines,
        )));
    }

    /// The opt-in was answered. `Yes` starts the search unchanged; the answer
    /// is deliberately **not** remembered, because a sticky opt-in is an
    /// opt-out.
    pub fn answer_remote_search(&mut self, allowed: bool) {
        let Some(request) = self.search.pending_remote.take() else {
            return;
        };
        if allowed {
            self.search.pending = Some(request);
        }
    }

    /// The listing a tab is showing, if it is showing one.
    pub fn listing(&self, side: Side, tab: usize) -> Option<Arc<ListFs>> {
        let id = self.panel(side).tab(tab)?.virtual_view()?.listing;
        self.router.listing(id)
    }

    /// the `Esc`: stop the walk, keep the hits.
    ///
    /// `false` when there was no walk running, which is what lets `Esc` fall
    /// through to its other meanings. The rows already
    /// found stay - "`Esc` stops the walk and keeps what was found" - and the
    /// panel's listing completes normally rather than reporting a failure.
    pub fn cancel_search(&mut self, side: Side) -> bool {
        let index = self.panel(side).active_index();
        let Some(listing) = self.listing(side, index) else {
            return false;
        };
        if listing.status().is_final() {
            return false;
        }
        listing.cancel();
        true
    }

    /// clear the virtual listing and return the panel to its
    /// underlying real directory. `false` when the panel is not virtual.
    ///
    /// The cursor lands on the name it was on when the listing was created,
    /// through the `pending_select` machinery that already exists for going up
    /// a directory. That is honestly not the hit you were looking at unless
    /// the hit lived in the origin directory: for a multi-root search there is
    /// no other single answer, and `Ctrl+Left` / `Ctrl+Right` is the key that
    /// takes you to a hit's own home.
    pub fn leave_virtual(&mut self, side: Side) -> bool {
        let index = self.panel(side).active_index();
        let Some(view) = self
            .panel(side)
            .tab(index)
            .and_then(|tab| tab.virtual_view().cloned())
        else {
            return false;
        };
        // `navigate_selecting` takes the view off the tab and forgets the
        // listing, which cancels it; doing it here as well would be two rules
        // for one thing.
        self.navigate_selecting(side, view.origin, view.origin_cursor);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::{app_with, connect_active, show_listing};
    use crate::app::{OpenRequest, ViewRequest};
    use crate::vfs::BackendKind;
    use crate::vfs::list::ListStatus;
    use crate::vfs::{Entry, VfsPath};

    /// Point a panel at a virtual listing through the same code `Alt+F7` uses,
    /// with no search engine behind it, and hand back its producer end.
    ///
    /// Everything below the compile-and-spawn step is what these tests are
    /// about: the design makes the results a `ListFs` in the panel, and the
    /// state machine of getting into and out of one has to hold whatever the
    /// walk did or did not find.
    /// the walk says once what it passed over, and says
    /// nothing at all when it read everything.
    #[test]
    fn a_finished_walk_reports_what_it_could_not_read_and_nothing_more() {
        let mut app = app_with(&["a"]);
        let sink = show_listing(&mut app, Side::Left, VirtualKind::Search, "[search: *]");
        let index = app.panel(Side::Left).active_index();
        let listing = app
            .panel(Side::Left)
            .tab(index)
            .and_then(Tab::virtual_view)
            .map(|view| view.listing)
            .expect("the tab is showing a listing");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("a runtime");
        let _guard = runtime.enter();
        let started = StartedSearch {
            side: Side::Left,
            listing,
            // Never awaited here: the handle is how the event loop reaches the
            // tally, and this is the half that decides what to do with one.
            walk: tokio::spawn(std::future::ready(Tally::default())),
        };

        // Nothing to report is silence, not a line saying nothing happened.
        app.report_search_tally(&started, &Tally::default());
        assert_eq!(app.message, None);

        let tally = Tally {
            visited: 2_181,
            unreadable: 3,
            first_problem: Some("/srv/private: permission denied".to_string()),
            ..Tally::default()
        };
        app.report_search_tally(&started, &tally);
        let message = app.message.clone().expect("a note about the walk");
        assert!(message.contains("2,181"), "{message}");
        assert!(message.contains("3 unreadable"), "{message}");

        // And an `Esc` outranks it: a stopped walk has already said how much it
        // kept, and a count of what it never reached is not news.
        app.message = None;
        sink.cancel();
        app.report_search_tally(&started, &tally);
        assert_eq!(app.message, None);
    }

    #[test]
    fn a_tab_is_virtual_exactly_when_its_path_is() {
        // The invariant every other rule here rests on: `virtual_view` is
        // `Some` if and only if the tab's path is a `list:` path, and the two
        // name the same listing.
        let mut app = app_with(&["a.rs", "b.rs"]);
        app.left.active_tab_mut().path = VfsPath::local("/root");
        assert!(!app.left.active_tab().is_virtual());

        let _sink = show_listing(
            &mut app,
            Side::Left,
            VirtualKind::Search,
            "[search: *.rs in /root]",
        );
        let tab = app.left.active_tab();
        assert!(tab.is_virtual());
        assert_eq!(tab.path.backend(), BackendKind::List);
        let view = tab.virtual_view().expect("a virtual tab has a view");
        assert_eq!(view.listing.to_path(), tab.path);
        assert_eq!(view.origin, VfsPath::local("/root"));
        assert_eq!(view.kind, VirtualKind::Search);
        assert!(tab.entries.is_empty(), "the rows arrive through the read");
        assert!(tab.loading, "and the panel says it is still filling");

        assert!(app.leave_virtual(Side::Left));
        let tab = app.left.active_tab();
        assert!(!tab.is_virtual());
        assert_eq!(tab.path, VfsPath::local("/root"));
        assert_eq!(tab.path.backend(), BackendKind::Local);
        assert!(
            !app.leave_virtual(Side::Left),
            "and there is nothing left to leave"
        );
    }

    #[test]
    fn leaving_a_virtual_listing_forgets_and_cancels_it() {
        // the design returns the panel to its real directory, and the walk
        // that was filling the listing has nothing left to fill: forgetting is
        // what stops it, so the two are one step and not two.
        let mut app = app_with(&["a.rs"]);
        app.left.active_tab_mut().path = VfsPath::local("/root");
        let sink = show_listing(
            &mut app,
            Side::Left,
            VirtualKind::Search,
            "[search: * in /root]",
        );
        assert_eq!(app.router.listing_count(), 1);
        assert!(sink.push(Entry::file("hit.rs")));

        assert!(app.leave_virtual(Side::Left));
        assert_eq!(app.router.listing_count(), 0);
        assert!(sink.is_cancelled(), "the walk is told to stop");
        assert!(!sink.push(Entry::file("late.rs")), "and cannot push again");
    }

    #[test]
    fn leaving_lands_the_cursor_where_the_search_started() {
        // the cursor lands "on the file that was selected",
        // through the same `pending_select` the parent-directory move uses -
        // it cannot be resolved now, because the listing has not been read.
        let mut app = app_with(&["a.rs", "b.rs"]);
        app.left.active_tab_mut().path = VfsPath::local("/root");
        app.left.active_tab_mut().cursor = 1;
        let _sink = show_listing(&mut app, Side::Left, VirtualKind::Branch, "[branch: /root]");
        assert!(app.leave_virtual(Side::Left));
        assert_eq!(
            app.left.active_tab().pending_select.as_deref(),
            Some("b.rs")
        );
    }

    #[test]
    fn a_second_search_into_one_tab_replaces_the_first_and_keeps_the_origin() {
        // one tab, one listing. The origin is the *real*
        // directory the tab came from, so searching from within a search still
        // knows where `Ctrl+R` goes back to.
        let mut app = app_with(&["a.rs"]);
        app.left.active_tab_mut().path = VfsPath::local("/root");
        let first = show_listing(
            &mut app,
            Side::Left,
            VirtualKind::Search,
            "[search: *.rs in /root]",
        );
        let second = show_listing(
            &mut app,
            Side::Left,
            VirtualKind::Search,
            "[search: *.md in /root]",
        );

        assert_eq!(app.router.listing_count(), 1, "the first is forgotten");
        assert!(first.is_cancelled());
        assert!(!second.is_cancelled());
        let view = app
            .left
            .active_tab()
            .virtual_view()
            .expect("still virtual")
            .clone();
        assert_eq!(view.origin, VfsPath::local("/root"));
        assert_eq!(view.header, "[search: *.md in /root]");

        assert!(app.leave_virtual(Side::Left));
        assert_eq!(app.left.active_tab().path, VfsPath::local("/root"));
        assert_eq!(app.router.listing_count(), 0);
    }

    #[test]
    fn entering_a_directory_from_a_virtual_listing_leaves_it() {
        // "Entering a directory from within a virtual listing
        // also leaves it, since the panel then has a real path." No branch of
        // its own: every navigation clears the view and forgets the listing.
        let mut app = app_with(&["a.rs"]);
        app.left.active_tab_mut().path = VfsPath::local("/root");
        let sink = show_listing(&mut app, Side::Left, VirtualKind::Branch, "[branch: /root]");

        app.navigate(Side::Left, VfsPath::local("/root/src"));
        assert!(!app.left.active_tab().is_virtual());
        assert_eq!(app.left.active_tab().path, VfsPath::local("/root/src"));
        assert_eq!(app.router.listing_count(), 0);
        assert!(sink.is_cancelled());
    }

    #[test]
    fn esc_stops_the_walk_and_keeps_what_was_found() {
        // "`Esc` stops the walk and keeps what was found." The
        // rows already pushed stay, the status is `Cancelled` rather than a
        // failure, and a second `Esc` has nothing left to stop - which is what
        // makes it mean "leave" instead.
        let mut app = app_with(&["a.rs"]);
        app.left.active_tab_mut().path = VfsPath::local("/root");
        let sink = show_listing(
            &mut app,
            Side::Left,
            VirtualKind::Search,
            "[search: * in /root]",
        );
        for name in ["one.rs", "two.rs", "three.rs"] {
            assert!(sink.push(Entry::file(name)));
        }

        assert!(app.cancel_search(Side::Left));
        let listing = app
            .listing(Side::Left, app.left.active_index())
            .expect("still registered");
        assert_eq!(listing.status(), ListStatus::Cancelled);
        assert_eq!(listing.len(), 3, "what was found is kept");
        assert_eq!(listing.entries().len(), 3);

        assert!(
            !app.cancel_search(Side::Left),
            "a second Esc has no walk to stop"
        );
        assert!(app.leave_virtual(Side::Left), "and leaves instead");
    }

    #[test]
    fn a_panel_that_is_not_virtual_has_nothing_to_stop_and_nothing_to_leave() {
        let mut app = app_with(&["a.rs"]);
        assert!(!app.cancel_search(Side::Left));
        assert!(!app.leave_virtual(Side::Left));
    }

    #[test]
    fn a_search_result_panel_restores_as_the_directory_it_came_from() {
        // a `list:/7` written to the state file names a
        // listing that will not exist next session, and the panel would open
        // on an error instead of on a directory.
        let mut app = app_with(&["a.rs"]);
        app.left.active_tab_mut().path = VfsPath::local("/root/deep");
        let _sink = show_listing(
            &mut app,
            Side::Left,
            VirtualKind::Search,
            "[search: * in /root/deep]",
        );
        let saved = crate::panel::state::snapshot(&app.left);
        let tab = saved.tabs.first().expect("one tab");
        assert_eq!(tab.path, "/root/deep");
    }

    #[test]
    fn enter_on_a_content_match_opens_the_viewer_at_the_hit() {
        // "For a content match, `Enter` opens the viewer at the
        // matching line with the hit already highlighted - by the same
        // `grep-regex` matcher that found it." The address is the row's real
        // home, not the `list:` path the panel is sitting at.
        let mut app = app_with(&[]);
        app.left.active_tab_mut().path = VfsPath::local("/root");
        let _sink = show_listing(
            &mut app,
            Side::Left,
            VirtualKind::Search,
            "[search: * \"TODO\" in /root]",
        );
        let find = crate::viewer::find::FindQuery {
            input: "TODO".to_string(),
            kind: crate::viewer::find::FindKind::Text,
            case: crate::config::QuickSearchCase::Sensitive,
        };
        if let Some(view) = app.left.active_tab_mut().virtual_view.as_mut() {
            view.find = Some(find.clone());
        }
        let home = VfsPath::local("/root/src/lib.rs");
        let mut row = Entry::file("lib.rs");
        row.location = Some(home.clone());
        row.hit = Some(Box::new(crate::vfs::ContentHit {
            offset: 4_096,
            decoded: false,
            line: Some(42),
            line_text: "// TODO: this".to_string(),
            charset: "UTF-8",
        }));
        app.left.active_tab_mut().entries = vec![row];

        app.open_under_cursor();
        match app.take_pending_view() {
            Some(ViewRequest::File { path, at }) => {
                assert_eq!(path, home, "the real file, not the listing");
                let at = at.expect("opened at the hit");
                assert_eq!(at.start, crate::viewer::HitStart::Offset(4_096));
                assert_eq!(at.line, Some(42));
                assert_eq!(at.find, Some(find), "the pattern that found it");
            }
            other => panic!("expected a viewer at the hit, got {other:?}"),
        }
        assert_eq!(app.message.as_deref(), Some("lib.rs: line 42"));
    }

    #[test]
    fn a_row_without_a_hit_is_opened_the_way_any_other_file_is() {
        // A name-only search result is a file like any other: the design
        // governs it, and nothing about the listing changes that.
        let mut app = app_with(&[]);
        app.left.active_tab_mut().path = VfsPath::local("/root");
        let _sink = show_listing(&mut app, Side::Left, VirtualKind::Branch, "[branch: /root]");
        let mut row = Entry::file("notes.txt");
        row.location = Some(VfsPath::local("/root/src/notes.txt"));
        app.left.active_tab_mut().entries = vec![row];

        app.open_under_cursor();
        // Queued for the event loop, at the row's **real** location: the
        // resolution reads the file's head and `dispatch` may not read. No
        // viewer is queued, because which of the three answers applies is not
        // known until it has.
        assert!(app.take_pending_view().is_none());
        assert_eq!(
            app.handoff.open,
            Some(OpenRequest::new(
                VfsPath::local("/root/src/notes.txt"),
                false
            ))
        );
    }

    /// I18: a content search over a remote root waits for the design's
    /// opt-in; a name-only one starts with no confirmation.
    #[test]
    fn a_remote_content_search_is_opt_in_and_a_name_search_is_not() {
        let mut app = app_with(&["a"]);
        let id = connect_active(&mut app);

        let names = Query::new(id.path("/srv"));
        app.request_search(names, VirtualKind::Search);
        assert!(app.take_pending_search().is_some(), "no prompt for names");
        assert!(!app.dialog_is_open());

        let mut content = Query::new(id.path("/srv"));
        content.content = Some(crate::search::query::ContentQuery {
            pattern: "TODO".to_string(),
            ..crate::search::query::ContentQuery::default()
        });
        app.request_search(content, VirtualKind::Search);
        assert!(
            app.take_pending_search().is_none(),
            "I18: the search waits for the answer"
        );
        assert_eq!(
            app.top_dialog().map(crate::dialog::Dialog::id),
            Some(DialogId::ConfirmRemoteSearch)
        );
        app.answer_remote_search(true);
        assert!(app.take_pending_search().is_some(), "yes starts it");
    }
}
