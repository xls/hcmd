//! The settings this session is running under.
//!
//! Two things a session is configured by and can change while it runs: the
//! sort the active tab is showing, and the three configuration documents.
//!
//! A reload replaces all three documents or none of them. `config.toml`,
//! `keymap.toml` and `theme.toml` are read together by
//! [`crate::config::load_from`], so a reload can never leave a half-updated
//! mixture of old and new behind - and a reload that cannot find the
//! configuration directory changes nothing at all, because the built-in
//! defaults are not what the user asked for when they pressed `Ctrl+Alt+R`.
//!
//! The reload itself is queued, never performed by
//! [`crate::input::dispatch`]: `Ctrl+Alt+R` sets a flag and the event loop
//! reads it, which is what keeps the input model free of the filesystem.

use crate::app::App;
use crate::config::{ColorDepth, Loaded, config_dir};
use crate::error::Result;
use crate::panel::{ColumnId, SortKey};
use crate::ui::dialog::theme::CatalogueAnswer;

impl App {
    /// `Ctrl+Shift+<n>`: set the secondary sort of the active tab, or reverse
    /// it if that column is already the secondary.
    pub fn sort_secondary(&mut self, column: ColumnId) {
        let directories_first = self.config.panel.directories_first;
        let ascii = self.config.ui.ascii_borders;
        let tab = self.active_panel_mut().active_tab_mut();
        tab.sort.apply_secondary(column);
        tab.sort_entries(directories_first);
        let tag = tab.sort.indicator(ascii);
        self.message = Some(format!("sorted {tag}"));
    }

    /// `Ctrl+Shift+0`: no secondary sort at all.
    ///
    /// A state the nine `Ctrl+Shift+<n>` keys cannot reach on their own -
    /// each of them *sets* a tiebreak, and none of them takes one away.
    pub fn sort_secondary_clear(&mut self) {
        let directories_first = self.config.panel.directories_first;
        let ascii = self.config.ui.ascii_borders;
        let tab = self.active_panel_mut().active_tab_mut();
        tab.sort.clear_secondary();
        tab.sort_entries(directories_first);
        let tag = tab.sort.indicator(ascii);
        self.message = Some(format!("sorted {tag}"));
    }

    /// Apply a sort key to the active tab. The same key again reverses.
    ///
    pub fn sort_active(&mut self, key: SortKey) {
        let directories_first = self.config.panel.directories_first;
        self.active_panel_mut()
            .sort_active_tab(key, directories_first);
    }

    /// `Ctrl+Alt+R`: ask the event loop to reload the configuration.
    ///
    /// A flag rather than the reload itself, so `dispatch` never touches the
    /// filesystem.
    pub fn reload_config(&mut self) {
        self.reload_requested = true;
    }

    /// Perform a queued reload. Called by the event loop, never by `dispatch`.
    ///
    /// A configuration directory that cannot be named is reported and nothing
    /// is touched. The alternative - and what this did - was to substitute
    /// [`Loaded::default`], which silently replaced the config, the keymap and
    /// the theme with the built-ins and then said "configuration reloaded":
    /// the one keystroke meant to re-read the user's files was the fastest way
    /// to lose them for the rest of the session.
    pub fn perform_reload(&mut self) -> Result<()> {
        self.reload_requested = false;
        match config_dir() {
            Ok(dir) => self.apply_reload(crate::config::load_from(&dir)),
            Err(err) => self.reload_unavailable(&err),
        }
        Ok(())
    }

    /// Say that a reload found nothing to read, and change nothing.
    ///
    /// Its own function so that the branch is reachable from a test: whether
    /// `config_dir` can fail depends on the environment the process was
    /// started in, and the promise being kept here - the running configuration
    /// survives - is not about the environment.
    fn reload_unavailable(&mut self, err: &crate::error::Error) {
        self.message = Some(format!("configuration not reloaded: {err}"));
    }

    /// Swap in three documents that were read together.
    ///
    /// The warnings are *appended*. They are the session's warning list, not
    /// the reload's: assigning here threw away everything since startup -
    /// every terminal capability that was missing, every file that would not
    /// parse - and the `F1` warning screen then claimed a clean start that
    /// never happened. Only the arrivals are counted in the message, because
    /// that is what this reload has to say.
    fn apply_reload(&mut self, loaded: Loaded) {
        // `[terminal] colors` can change under a reload, so the depth is
        // re-resolved rather than left at whatever startup detected.
        self.color_depth = ColorDepth::resolve(loaded.config.terminal.colors);
        self.config = loaded.config;
        self.keymap = loaded.keymap;
        self.theme = loaded.theme;
        let arrived = loaded.warnings.len();
        self.warnings.extend(loaded.warnings);
        self.message = Some(if arrived == 0 {
            "configuration reloaded".to_string()
        } else {
            format!("configuration reloaded with {arrived} warning(s)")
        });
    }
}

/// Ask GitHub what themes the repository has, on the blocking pool.
///
/// The same shape as every other question this program asks of the network: a
/// blocking call on a worker, the answer down a channel, and nobody waiting
/// on it. `None` when there is no runtime to spawn on - a headless test - in
/// which case the picker simply never hears back, which is a state it has to
/// handle anyway.
fn ask_the_repository() -> Option<std::sync::mpsc::Receiver<CatalogueAnswer>> {
    let handle = tokio::runtime::Handle::try_current().ok()?;
    let (tx, rx) = std::sync::mpsc::channel();
    handle.spawn_blocking(move || {
        let answer = crate::config::catalogue::fetch_names().map_err(|e| e.to_string());
        // The picker was closed before the answer came. Nothing to tell.
        let _ = tx.send(answer);
    });
    Some(rx)
}

impl App {
    /// `Alt+T`: open the theme picker.
    ///
    /// The list is every shipped name, every `themes/<name>.toml` in the
    /// configuration directory, and the current one if it came from somewhere
    /// else again, so a hand-written theme is reachable rather than invisible.
    /// The repository is asked as well, but on a worker thread: the list is
    /// on screen before the question is even sent, and the names it does not
    /// have yet arrive in [`Self::service_theme_preview`] whenever they
    /// arrive.
    pub fn open_theme_picker(&mut self) {
        let mut names: Vec<String> = crate::config::available_theme_names();
        let current = self.theme.name.clone();
        if !names.contains(&current) {
            names.push(current.clone());
            names.sort();
        }
        let picker = crate::ui::dialog::theme::ThemeDialog::new(names, &current).with_quick_search(
            self.config.panel.quick_search,
            self.config.panel.quick_search_case,
        );
        let picker = match ask_the_repository() {
            Some(incoming) => picker.expecting(incoming),
            None => picker,
        };
        self.push_dialog(Box::new(picker));
    }

    /// The picker on top of the stack, if that is what is on top of it.
    fn theme_picker_mut(&mut self) -> Option<&mut crate::ui::dialog::theme::ThemeDialog> {
        let dialog = self.top_dialog_mut()?;
        if dialog.id() != crate::input::DialogId::Theme {
            return None;
        }
        dialog.as_any_mut()?.downcast_mut()
    }

    /// Fold the repository's answer into the open picker.
    ///
    /// A failure is a line in the status bar and nothing else. The picker is
    /// still a working picker over everything on disk, so there is nothing to
    /// undo and nothing to retry.
    fn service_theme_catalogue(&mut self) {
        let Some(answer) = self
            .theme_picker_mut()
            .and_then(crate::ui::dialog::theme::ThemeDialog::take_answer)
        else {
            return;
        };
        let names = match answer {
            Ok(names) => names,
            Err(why) => {
                self.message = Some(format!("themes: could not ask the repository - {why}"));
                return;
            }
        };
        let added = self
            .theme_picker_mut()
            .map_or(0, |picker| picker.offer_remote(names));
        if added > 0 {
            self.message = Some(format!(
                "themes: {added} more in the repository, marked + - Enter fetches one"
            ));
        }
    }

    /// Keep the running theme in step with the picker's cursor.
    ///
    /// Called once a frame, before the draw. The picker owns which name is
    /// selected and nothing else; swapping the theme is the application's to
    /// do, so the dialog stays a list and this stays the only place a theme
    /// changes for a reason other than a reload.
    ///
    /// A name that will not load leaves the theme alone rather than falling
    /// back to blue: the cursor is somewhere the user can see, and swapping to
    /// a third theme would be a worse answer than showing the one they have.
    pub fn service_theme_preview(&mut self) {
        self.service_theme_catalogue();
        let Some(dialog) = self.top_dialog() else {
            return;
        };
        if dialog.id() != crate::input::DialogId::Theme {
            return;
        }
        let Some(picker) = dialog
            .as_any()
            .and_then(|any| any.downcast_ref::<crate::ui::dialog::theme::ThemeDialog>())
        else {
            return;
        };
        let Some(wanted) = picker.selected() else {
            return;
        };
        // A name the repository offered is not on this machine, so there is
        // nothing to apply. The screen keeps the last theme that could be.
        if picker.is_remote_only(wanted) {
            return;
        }
        if wanted == self.theme.name {
            return;
        }
        let Some(text) = crate::config::builtin_theme(wanted) else {
            return;
        };
        let (theme, _warnings) = crate::config::Theme::parse(text, wanted);
        self.theme = theme;
    }

    /// Apply a theme that is on this machine, by name.
    ///
    /// `themes/<name>.toml` first and the compiled-in set second, which is
    /// the order the loader uses. `false` when there is no such theme, so the
    /// caller can say so rather than the screen silently not changing.
    ///
    /// The preview path does not go through this: it applies built-ins only,
    /// and only ever a name that is already in the list. This is for a theme
    /// that has just been fetched, and there is no built-in of that name.
    pub fn adopt_theme(&mut self, name: &str) -> bool {
        if self.theme.name == name {
            return true;
        }
        let dir = config_dir().ok();
        let Some(theme) = crate::config::catalogue::installed_theme(dir.as_deref(), name) else {
            return false;
        };
        self.theme = theme;
        true
    }

    /// Put back the theme the picker opened on.
    ///
    /// `Esc` in the picker, so that looking through the list costs nothing.
    pub fn restore_theme(&mut self, name: &str) {
        if name == self.theme.name {
            return;
        }
        if let Some(text) = crate::config::builtin_theme(name) {
            let (theme, _warnings) = crate::config::Theme::parse(text, name);
            self.theme = theme;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Keymap, Theme};
    use crate::panel::SortState;
    use crate::vfs::Entry;

    #[test]
    fn sorting_is_stable_and_puts_the_parent_row_first() {
        let mut app = App::headless(Config::default(), Keymap::builtin(), Theme::blue());
        let mut b = Entry::dir("bee");
        b.size = 10;
        let mut a = Entry::file("Ant");
        a.size = 30;
        let mut c = Entry::file("cat");
        c.size = 20;
        app.left.active_tab_mut().entries = vec![c, Entry::parent_entry(), a, b];
        app.left.active_tab_mut().sort = SortState {
            key: SortKey::Unsorted,
            reverse: false,
            secondary: None,
        };

        app.sort_active(SortKey::Column(ColumnId::Name));
        let names: Vec<&str> = app
            .left
            .active_tab()
            .entries
            .iter()
            .map(|e| e.name.as_str())
            .collect();
        assert_eq!(
            names,
            ["..", "bee", "Ant", "cat"],
            "parent first, then directories, then case-insensitive by name"
        );

        // The same key again reverses, and `..` stays put.
        app.sort_active(SortKey::Column(ColumnId::Name));
        assert!(app.left.active_tab().sort.reverse);
        let names: Vec<&str> = app
            .left
            .active_tab()
            .entries
            .iter()
            .map(|e| e.name.as_str())
            .collect();
        assert_eq!(names, ["..", "bee", "cat", "Ant"]);
    }

    #[test]
    fn a_reload_is_queued_rather_than_read_by_dispatch() {
        let mut app = App::headless(Config::default(), Keymap::builtin(), Theme::blue());
        assert!(!app.reload_requested);
        app.reload_config();
        assert!(app.reload_requested, "a flag, not a config read");
    }

    #[test]
    fn a_reload_keeps_the_warnings_the_session_has_already_collected() {
        // The warning list belongs to the session. Assigning the reload's own
        // list over it erased every warning since startup and reported a clean
        // configuration that was never loaded.
        let mut app = App::headless(Config::default(), Keymap::builtin(), Theme::blue());
        app.warnings.push("a warning from startup".to_string());

        let mut loaded = Loaded::default();
        loaded
            .warnings
            .push("a warning from the reload".to_string());
        app.apply_reload(loaded);

        assert_eq!(
            app.warnings,
            vec![
                "a warning from startup".to_string(),
                "a warning from the reload".to_string()
            ],
            "the reload appends to the session's warnings"
        );
        assert_eq!(
            app.message.as_deref(),
            Some("configuration reloaded with 1 warning(s)"),
            "and counts what this reload brought, not the session total"
        );
    }

    #[test]
    fn a_reload_that_cannot_read_anything_changes_nothing() {
        // `Ctrl+Alt+R` with no configuration directory used to install the
        // built-in config, keymap and theme, set the warnings to none and
        // report "configuration reloaded". There is nothing to install, so
        // this says so and leaves the session exactly as it was.
        let mut app = App::headless(Config::default(), Keymap::builtin(), Theme::blue());
        app.config.panel.max_tabs = 3;
        app.warnings.push("a warning from startup".to_string());

        app.reload_unavailable(&crate::error::Error::msg("$HOME is not set"));

        assert_eq!(app.config.panel.max_tabs, 3, "the running config stands");
        assert_eq!(app.warnings, vec!["a warning from startup".to_string()]);
        let message = app.message.as_deref().unwrap_or_default();
        assert!(
            message.starts_with("configuration not reloaded"),
            "{message}"
        );
        assert!(message.contains("$HOME is not set"), "{message}");
    }

    #[test]
    fn a_reload_replaces_all_three_documents_or_none_of_them() {
        let mut app = App::headless(Config::default(), Keymap::builtin(), Theme::blue());
        app.reload_config();
        // Whatever is on this machine, the three documents and the warnings
        // that came with them agree with each other afterwards, and the flag
        // that asked for the reload is down.
        app.perform_reload().expect("reload");
        assert!(!app.reload_requested);
        assert!(app.message.is_some(), "the reload says that it happened");
    }
}

#[cfg(test)]
mod theme_picker_tests {
    use crate::app::tests::app_with;
    use crate::input::{DialogId, KeyCode, KeyPress};

    /// Drive the open picker with one key, the way the input layer does.
    fn press(app: &mut crate::app::App, code: KeyCode) {
        let key = crate::dialog::DialogKey::raw(KeyPress::plain(code));
        if let Some(dialog) = app.top_dialog_mut() {
            let _ = dialog.handle_key(&key);
        }
        app.service_theme_preview();
    }

    #[test]
    fn moving_the_cursor_changes_the_running_theme() {
        // The whole point of the picker: the preview IS the selection. This
        // failed silently once and cost a round trip to find, because
        // `Dialog::as_any` defaults to `None` and the downcast the event loop
        // does simply returned nothing - no panic, no compile error, just a
        // list that did not do anything. Asserting the theme actually changes
        // is what catches that.
        let mut app = app_with(&["a.txt"]);
        app.open_theme_picker();
        assert_eq!(
            app.top_dialog().map(crate::dialog::Dialog::id),
            Some(DialogId::Theme)
        );
        let opened_on = app.theme.name.clone();

        // Somewhere else in the list, whichever direction has room.
        press(&mut app, KeyCode::Down);
        let moved = app.theme.name.clone();
        if moved == opened_on {
            press(&mut app, KeyCode::Up);
        }
        assert_ne!(
            app.theme.name, opened_on,
            "moving the cursor did not change the theme"
        );
    }

    #[test]
    fn esc_puts_back_the_theme_it_opened_on() {
        // Looking through the list has to cost nothing, or nobody will look.
        let mut app = app_with(&["a.txt"]);
        let before = app.theme.name.clone();
        app.open_theme_picker();
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Down);
        assert_ne!(app.theme.name, before, "the preview moved");

        // What `DialogOutcome::Cancel` does in `input::dialogs`.
        let restore = app
            .top_dialog()
            .and_then(crate::dialog::Dialog::as_any)
            .and_then(|any| any.downcast_ref::<crate::ui::dialog::theme::ThemeDialog>())
            .map(|p| p.original().to_string())
            .expect("the picker answers which theme it opened on");
        app.pop_dialog();
        app.restore_theme(&restore);
        assert_eq!(app.theme.name, before, "Esc put back what was running");
    }

    #[test]
    fn typing_quick_searches_the_names_and_previews_the_match() {
        let mut app = app_with(&["a.txt"]);
        app.open_theme_picker();
        for c in "nord".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        assert_eq!(app.theme.name, "nord", "typing a name previewed that theme");
    }
    /// The open picker, for a test that needs to reach past the trait.
    fn picker(app: &mut crate::app::App) -> &mut crate::ui::dialog::theme::ThemeDialog {
        app.top_dialog_mut()
            .and_then(|d| d.as_any_mut())
            .and_then(|any| any.downcast_mut::<crate::ui::dialog::theme::ThemeDialog>())
            .expect("the theme picker is on top")
    }

    #[test]
    fn a_theme_only_the_repository_has_is_offered_and_marked() {
        let mut app = app_with(&["a.txt"]);
        app.open_theme_picker();
        let added = picker(&mut app).offer_remote(vec![
            // Already here, so not an offer at all.
            "nord".to_string(),
            "midnight-oil".to_string(),
        ]);
        assert_eq!(added, 1, "only the name this machine lacks is added");
        let p = picker(&mut app);
        assert!(p.is_remote_only("midnight-oil"));
        assert!(!p.is_remote_only("nord"), "a shipped theme is not an offer");
    }

    #[test]
    fn the_cursor_stays_on_its_name_when_the_list_grows_under_it() {
        // The list is sorted, so names arriving move every row after them. A
        // cursor left on its row index would preview a different theme than
        // the one the user is looking at.
        let mut app = app_with(&["a.txt"]);
        app.open_theme_picker();
        for c in "nord".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        let p = picker(&mut app);
        p.offer_remote(vec!["aaaa-first".to_string(), "bbbb-second".to_string()]);
        assert_eq!(p.selected(), Some("nord"));
        assert_eq!(app.theme.name, "nord");
    }

    #[test]
    fn moving_onto_one_that_is_not_here_yet_leaves_the_screen_alone() {
        // There is nothing to apply, and swapping to a third theme because
        // the cursor passed over a name would be worse than doing nothing.
        let mut app = app_with(&["a.txt"]);
        app.open_theme_picker();
        for c in "nord".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        assert_eq!(app.theme.name, "nord");
        picker(&mut app).offer_remote(vec!["nord-zzz".to_string()]);
        press(&mut app, KeyCode::Down);
        let p = picker(&mut app);
        assert_eq!(p.selected(), Some("nord-zzz"), "the cursor did move");
        assert_eq!(app.theme.name, "nord", "and the theme did not");
    }

    #[test]
    fn quick_search_matches_the_name_and_not_the_marker() {
        let mut app = app_with(&["a.txt"]);
        app.open_theme_picker();
        picker(&mut app).offer_remote(vec!["zephyr".to_string()]);
        for c in "zephyr".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        assert_eq!(
            picker(&mut app).selected(),
            Some("zephyr"),
            "the plain name is what is matched and what is selected"
        );
    }

    /// A picker with the repository's answer already waiting on its channel,
    /// which is what the worker leaves behind by the time a frame is drawn.
    ///
    /// Opened on the theme the session is actually running, exactly as
    /// `open_theme_picker` opens it, so the cursor starts where the screen
    /// already is and a preview of the first frame changes nothing. A fixture
    /// that opened on some other name would make every assertion below about
    /// the fixture rather than about what arrived.
    fn picker_answered(
        app: &mut crate::app::App,
        answer: crate::ui::dialog::theme::CatalogueAnswer,
    ) -> String {
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(answer).expect("the receiver is alive");
        let current = app.theme.name.clone();
        let names = vec![current.clone(), "nord".to_string()];
        let dialog = crate::ui::dialog::theme::ThemeDialog::new(names, &current).expecting(rx);
        app.push_dialog(Box::new(dialog));
        current
    }

    #[test]
    fn a_repository_that_cannot_be_reached_only_says_so() {
        // Everything on disk still works. The failure is a line in the status
        // bar and nothing else.
        let mut app = app_with(&["a.txt"]);
        let before = app.theme.name.clone();
        let opened_on = picker_answered(&mut app, Err("no route to host".to_string()));
        app.service_theme_preview();
        assert!(
            app.message
                .as_deref()
                .is_some_and(|m| m.contains("no route")),
            "{:?}",
            app.message
        );
        assert_eq!(app.theme.name, before, "nothing was applied");
        assert_eq!(
            picker(&mut app).selected(),
            Some(opened_on.as_str()),
            "the list is still the list that was on disk"
        );
    }

    #[test]
    fn the_answer_is_read_once_and_the_names_appear() {
        let mut app = app_with(&["a.txt"]);
        let opened_on = picker_answered(
            &mut app,
            Ok(vec!["nord".to_string(), "quicksand".to_string()]),
        );
        app.service_theme_preview();
        assert!(
            app.message.as_deref().is_some_and(|m| m.contains("1 more")),
            "{:?}",
            app.message
        );
        let dialog = picker(&mut app);
        assert!(dialog.is_remote_only("quicksand"));
        assert!(!dialog.is_remote_only("nord"), "nord was already here");
        assert_eq!(
            dialog.selected(),
            Some(opened_on.as_str()),
            "the cursor did not move when the list grew"
        );

        // A second frame has nothing to read and must not say anything again.
        app.message = None;
        app.service_theme_preview();
        assert_eq!(app.message, None);
    }
}
