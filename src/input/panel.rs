//! Keys that act on a panel's listing.
//!
//! Quick search, the `+` / `-` mask prompts, the size walks and the four ways
//! of putting a listing back the way it was. What they have in common is the
//! rows: each of them changes which rows a panel shows, which of them are
//! marked, or what is known about them, and none of them opens a dialog that
//! turns into a job.
//!
//! `Esc` unwinds in a fixed order rather than doing one thing: a running quick
//! search first, then the marks, then a virtual listing. That order is what
//! makes `Esc` safe to press, because the cheapest thing to undo goes first.

use crate::app::App;
use crate::config::DigitKeys;
use crate::input::{DialogId, quicksearch};
use crate::ops::JobId;
use crate::ops::JobKind;
use crate::panel::mask::MaskDialog;
use crate::vfs::VfsPath;

/// A printable key with a panel focused.
///
/// Bare digits feed the buffer like any other character - that is the whole
/// point of the design, since `2026-budget.xlsx` has to be reachable by its
/// first character. `panel.digit_keys = "tabs"` swaps it back: `1`-`9` switch
/// tabs *while the buffer is empty*, and `Ctrl+S` is then the explicit way in.
pub(super) fn panel_printable(app: &mut App, c: char) {
    if app.config.panel.digit_keys == DigitKeys::Tabs
        && !app.active_panel().quick.is_active()
        && let Some(n) = c.to_digit(10)
        && (1..=9).contains(&n)
    {
        select_tab(app, n as usize);
        return;
    }
    type_into_quick_search(app, c);
}

/// Append to the quick-search buffer and move the cursor to the first match.
///
///
/// On no match the buffer is **kept** and the panel status flashes, so the next
/// `Backspace` returns to the last query that did match rather than starting
/// over.
pub(super) fn type_into_quick_search(app: &mut App, c: char) {
    app.active_panel_mut().quick.push(c);
    rematch(app);
}

/// Move the cursor to the first entry matching the current buffer, or flash.
pub(super) fn rematch(app: &mut App) {
    let query = app.active_panel().quick.buffer.clone();
    if query.is_empty() {
        return;
    }
    if !app.move_cursor_to_match(&query) {
        let case = app.config.panel.quick_search_case;
        app.message = Some(format!(
            "no match: {query} {}",
            quicksearch::case_indicator(&query, case)
        ));
    }
}

/// `Space` on a directory: walk it to full depth and show its size.
///
///
/// This is the whole of what makes `Space` differ from `Insert`. Only the entry
/// under the cursor is walked - `Insert`, `+`, `*` and `Ctrl+A` deliberately
/// never walk anything, because `Ctrl+A` in `/` would stat the filesystem.
///
/// A directory that is already in the cache costs nothing:
/// [`App::request_size`] filters it out.
pub(super) fn size_under_cursor(app: &mut App) {
    let tab = app.active_panel().active_tab();
    let Some(entry) = tab.current() else { return };
    if !entry.is_dir() || entry.is_parent {
        return;
    }
    let Some(path) = tab.current_path() else {
        return;
    };
    app.request_size(vec![path]);
}

/// `Ctrl+L`: "calculate occupied space of selection".
///
/// the design is specific about what this is for: it "walks every marked
/// directory and resolves the figure, after which the `\u{2265}` disappears".
/// With nothing marked it sizes the entry under the cursor, which is what makes
/// it useful without marking anything first.
pub(super) fn size_selection(app: &mut App) {
    let tab = app.active_panel().active_tab();
    let marked: Vec<VfsPath> = tab
        .entries
        .iter()
        .filter(|e| !e.is_parent && e.is_dir() && tab.marks.contains(&e.name))
        .map(|e| tab.path.join(&e.name))
        .collect();

    let paths = if marked.is_empty() {
        match tab.current() {
            Some(entry) if entry.is_dir() && !entry.is_parent => {
                tab.current_path().into_iter().collect()
            }
            _ => Vec::new(),
        }
    } else {
        marked
    };

    if paths.is_empty() {
        app.message = Some("nothing to size: mark a directory first".to_string());
        return;
    }
    let count = paths.len();
    if app.request_size(paths).is_none() {
        app.message = Some(format!(
            "{count} director{} already sized",
            if count == 1 { "y is" } else { "ies are" }
        ));
    }
}

/// `+` / `-`: open the mask prompt.
///
/// All three of the prompt's controls "persist for the session" and the mask is shared
/// between the two keys: the mask is [`crate::panel::mask::History::last`], starting at `*`,
/// and the `Exclude directories` checkbox is its `exclude_dirs`, starting off. Marking by
/// mask never walks a tree: only `Space` and `Ctrl+L` size anything.
pub(super) fn open_mask_prompt(app: &mut App, id: DialogId) {
    let initial = app.masks.last.clone();
    let dialog = MaskDialog::new(id, initial).excluding_dirs(app.masks.exclude_dirs);
    app.push_dialog(Box::new(dialog));
}

/// `Alt+<n>`, or a bare digit under `panel.digit_keys = "tabs"`.
///
/// A tab switch changes which directory is active without reading one, so
/// the panel → shell half has to be told here: `navigate` is the
/// only other place that tells it, and a tab switch never goes through it.
pub(super) fn select_tab(app: &mut App, n: usize) {
    if !app.active_panel_mut().select_tab(n.saturating_sub(1)) {
        app.message = Some(format!("there is no tab {n}"));
        return;
    }
    app.sync_active_cwd();
}

/// `Esc` on a panel: clear the quick-search buffer; if it is already empty,
/// clear the selection.
///
/// The branch is on the *buffer*, as the spec writes it - an `Esc` on a panel
/// with nothing typed clears the marks. But `Ctrl+S` can leave the search
/// **armed** with an empty buffer, and the design says the buffer "is cleared
/// by `Esc`, by cursor movement, or by leaving the directory": a search the
/// status line is visibly showing has to end when `Esc` is pressed, whichever
/// branch is taken, or `search: [aa]` stays on screen with no key that dismisses
/// it.
pub(super) fn clear_search_then_marks(app: &mut App) {
    let side = app.active_side;
    // "`Esc` stops the walk and keeps what was found." First,
    // ahead of the size walk that used to hold this place: a user watching a
    // search fill and pressing `Esc` means the thing on the screen, and a
    // background size walk is not on the screen. The rows already found stay,
    // and a second `Esc` then *leaves* the listing (step 4), which is how
    // the design both hold of the same key.
    if app.cancel_search(side) {
        let index = app.panel(side).active_index();
        let kept = app.listing(side, index).map_or(0, |listing| listing.len());
        app.message = Some(format!("search stopped; {kept} kept"));
        return;
    }
    // "`Esc` abandons the walk leaving the bound in place". The bound
    // stays because a cancelled walk never enters the size cache (the last
    // bullet).
    let walking: Vec<JobId> = app
        .jobs
        .rows()
        .iter()
        .filter(|j| j.kind == JobKind::Size && j.finished.is_none())
        .map(|j| j.id)
        .collect();
    if !walking.is_empty() {
        let count = walking.len();
        for id in walking {
            app.cancel_job(id);
        }
        app.message = Some(format!(
            "{count} size walk{} abandoned",
            if count == 1 { "" } else { "s" }
        ));
        return;
    }

    let panel = app.active_panel_mut();
    let had_buffer = !panel.quick.is_empty();
    panel.quick.clear();
    if had_buffer {
        return;
    }
    // A half-typed command line is a thing on the screen with something in
    // it, and `Esc` is the key that empties what is on the screen. Before the
    // virtual listing and before the marks, for the same reason the quick
    // search comes before both: the nearer the state is to the cursor, the
    // sooner `Esc` should reach it.
    if clear_command_line(app) {
        return;
    }
    // "`Esc` on a virtual panel does the same thing" as
    // `Ctrl+R` - it leaves. Before the marks, because a panel showing search
    // results is a state the user can see and a set of marks is not.
    if leave_virtual(app) {
        return;
    }
    app.active_panel_mut().active_tab_mut().marks.clear();
}

/// Empty the command line, whoever owns it. True when there was something to
/// empty.
///
/// With a shell running, the text belongs to the shell and not to this
/// program, so it is emptied the way a person would: `Ctrl+U`, the kill-line
/// the shell's own line editor answers. Clearing a buffer here instead would
/// leave the shell still holding the characters and the next `Enter` would run
/// them.
///
/// Without a shell, the built-in command line holds the text and clearing it
/// is clearing it.
pub(super) fn clear_command_line(app: &mut App) -> bool {
    if app.console_owns_cmdline() {
        // `None` is "cannot tell", which is not "empty": a shell whose prompt
        // this program cannot read may still be holding a half-typed line, and
        // a kill-line that arrives at an already-empty prompt costs nothing.
        if app
            .console
            .shell
            .as_ref()
            .and_then(crate::console::Console::input_is_empty)
            == Some(true)
        {
            return false;
        }
        // 0x15 is `Ctrl+U`. Typed, because a person pressed a key: the console
        // draws it without the delay a generated write gets.
        app.console.queue(&[0x15], crate::console::Origin::Typed);
        return true;
    }
    if app.cmdline.text().is_empty() {
        return false;
    }
    app.cmdline.clear();
    true
}

/// the "leave the virtual listing", with the line that says so.
///
/// The one place `Ctrl+R` and `Esc` share, so the two keys cannot come to
/// differ about what leaving means. False when the panel is a real directory,
/// which is what lets each key fall through to its own other meaning.
pub(super) fn leave_virtual(app: &mut App) -> bool {
    let side = app.active_side;
    let index = app.panel(side).active_index();
    let kind = app
        .panel(side)
        .tab(index)
        .and_then(crate::panel::Tab::virtual_view)
        .map(|view| view.kind);
    if !app.leave_virtual(side) {
        return false;
    }
    if let Some(kind) = kind {
        app.message = Some(format!("left the {} listing", kind.id()));
    }
    true
}

/// Re-read the active panel (`F2`, `Ctrl+R`, and the re-read half of
/// `Ctrl+H`).
///
/// [`App::reread`] rather than a `navigate` to the same path: the design
/// keeps the selection across a re-read and clears it only on a directory
/// change.
pub(super) fn reread(app: &mut App) {
    // "`Ctrl+R` on a virtual panel clears it and returns the
    // panel to its underlying real directory" - "one key, resolved by panel
    // state". There is nothing to re-read on a search result: the walk is
    // over, and running it again is what `Alt+F7` is for.
    if leave_virtual(app) {
        return;
    }
    let side = app.active_side;
    // The third state-dependent meaning of the same key: a dropped connection
    // reconnects. the design writes `F2` here and the design settle `F2` as
    // pure rename and reread as `Ctrl+R`, and this contract follows the later
    // and more specific pair.
    if app.reconnect(side) {
        return;
    }
    // A live remote listing is served from a cache with a short TTL,
    // so a reread has to drop it first or it would show the
    // same rows back.
    invalidate_remote_listing(app, side);
    app.reread(side);
}

/// Drop the cached listing for the directory a remote tab is showing, so
/// `Ctrl+R` is a real reread.
pub(super) fn invalidate_remote_listing(app: &mut App, side: crate::panel::Side) {
    let path = app.panel(side).active_tab().path.clone();
    let Some(id) = crate::remote::RemoteId::from_path(&path) else {
        return;
    };
    if let Some(fs) = app.remote_fs(id) {
        fs.invalidate_path(&path);
    }
}
