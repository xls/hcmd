//! The the design input model, driven exactly the way the event loop drives it:
//! synthetic [`KeyEvent`]s through [`dispatch`], assertions on [`App`] state.
//!
//! Everything here goes through the crate's public API and needs no terminal,
//! no filesystem and no test-only hooks in the production paths - which is the
//! property `App::headless` exists to give.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "an integration test is its own crate, so it is not #[cfg(test)] \
              and clippy.toml's allow-*-in-tests keys do not reach it. \
              Panicking assertions are the point of a test."
)]

use holoscommander::app::App;
use holoscommander::config::{
    Config, DigitKeys, Keymap, QuickSearchCase, QuickSearchMode, TabBar, Theme,
};
use holoscommander::input::{
    Focus, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, dispatch, panel_status,
};
use holoscommander::panel::{ColumnId, Side, SortKey};
use holoscommander::vfs::{Entry, VfsPath};
use ratatui::layout::Rect;

const NONE: KeyModifiers = KeyModifiers::NONE;
const CTRL: KeyModifiers = KeyModifiers::CONTROL;
const ALT: KeyModifiers = KeyModifiers::ALT;
const SHIFT: KeyModifiers = KeyModifiers::SHIFT;

/// A headless app whose left panel sits in a directory with a parent (so
/// `Backspace` has somewhere to go) and holds `names`.
fn app_with(names: &[&str]) -> App {
    app_with_config(Config::default(), names)
}

fn app_with_config(config: Config, names: &[&str]) -> App {
    let mut app = App::headless(config, Keymap::builtin(), Theme::blue());
    app.navigate(Side::Left, VfsPath::local("/home/thorin/src"));
    let _ = app.take_pending_reads();
    let tab = app.left.active_tab_mut();
    tab.entries = names.iter().map(|n| Entry::file(*n)).collect();
    tab.loading = false;
    tab.cursor = 0;
    app
}

fn press(app: &mut App, code: KeyCode, mods: KeyModifiers) {
    dispatch(app, KeyEvent::new(code, mods)).expect("dispatch never fails on a synthetic key");
}

fn type_text(app: &mut App, text: &str) {
    for c in text.chars() {
        // A terminal reports an uppercase letter with SHIFT held; crossterm
        // adds it too. Feed it the same way, so normalisation is exercised.
        let mods = if c.is_uppercase() { SHIFT } else { NONE };
        press(app, KeyCode::Char(c), mods);
    }
}

fn cursor(app: &App) -> usize {
    app.left.active_tab().cursor
}

fn cursor_name(app: &App) -> String {
    app.left
        .active_tab()
        .current()
        .map(|e| e.name.clone())
        .unwrap_or_default()
}

fn buffer(app: &App) -> String {
    app.left.quick.buffer.clone()
}

// ---------------------------------------------------------------- quick search

#[test]
fn the_first_character_typed_is_part_of_the_buffer() {
    // the design acceptance criterion: no activation key, and typing "tho"
    // lands on "thorin" with the "t" included.
    let mut app = app_with(&["alpha", "beta", "thorin", "zeta"]);
    type_text(&mut app, "t");
    assert_eq!(
        buffer(&app),
        "t",
        "the very first key is already the buffer"
    );
    assert_eq!(cursor_name(&app), "thorin");
    type_text(&mut app, "ho");
    assert_eq!(buffer(&app), "tho");
    assert_eq!(cursor_name(&app), "thorin");
    assert_eq!(
        panel_status(&app.left, app.config.panel.quick_search_case).as_deref(),
        Some("search: tho [aa]")
    );
}

#[test]
fn free_typing_never_reaches_the_command_line() {
    let mut app = app_with(&["thorin"]);
    type_text(&mut app, "tho");
    assert!(
        app.cmdline.is_empty(),
        "typing navigates, it does not compose"
    );
    assert_eq!(app.focus, Focus::Panel(Side::Left));
}

#[test]
fn smart_case_is_insensitive_until_an_uppercase_character_is_typed() {
    // "tho" matches Thorin and thorin; "Tho" matches only Thorin.
    let mut app = app_with(&["alpha", "Thorin", "thorin"]);
    type_text(&mut app, "tho");
    assert_eq!(cursor_name(&app), "Thorin", "insensitive: the first match");

    let mut app = app_with(&["alpha", "thorin", "Thorin"]);
    type_text(&mut app, "Tho");
    assert_eq!(buffer(&app), "Tho", "the case that was typed is kept");
    assert_eq!(
        cursor_name(&app),
        "Thorin",
        "sensitive the moment T is typed"
    );
    assert_eq!(
        panel_status(&app.left, app.config.panel.quick_search_case).as_deref(),
        Some("search: Tho [Aa]")
    );
}

#[test]
fn explicit_case_settings_override_the_smart_rule() {
    let mut config = Config::default();
    config.panel.quick_search_case = QuickSearchCase::Sensitive;
    let mut app = app_with_config(config, &["Thorin", "thorin"]);
    type_text(&mut app, "tho");
    assert_eq!(cursor_name(&app), "thorin", "sensitive skips Thorin");

    let mut config = Config::default();
    config.panel.quick_search_case = QuickSearchCase::Insensitive;
    let mut app = app_with_config(config, &["thorin", "Thorin"]);
    type_text(&mut app, "Tho");
    assert_eq!(cursor_name(&app), "thorin", "insensitive takes the first");
}

#[test]
fn prefix_substring_and_fuzzy_select_what_matches() {
    let names = ["alpha", "my-thorin.txt", "thorin"];

    let mut app = app_with(&names);
    type_text(&mut app, "tho");
    assert_eq!(cursor_name(&app), "thorin", "prefix skips my-thorin.txt");

    let mut config = Config::default();
    config.panel.quick_search = QuickSearchMode::Substring;
    let mut app = app_with_config(config, &names);
    type_text(&mut app, "tho");
    assert_eq!(
        cursor_name(&app),
        "my-thorin.txt",
        "substring matches inside"
    );

    let mut config = Config::default();
    config.panel.quick_search = QuickSearchMode::Fuzzy;
    let mut app = app_with_config(config, &["alpha", "thorin"]);
    type_text(&mut app, "tms");
    assert_eq!(cursor_name(&app), "thorin", "fuzzy is a subsequence");
}

/// A character that would match nothing is refused, not typed.
///
/// This asserted the opposite until 2026-08-30: the character was appended and
/// the status flashed, on the reasoning that `Backspace` would return to the
/// last query that matched. Refusing it means never having left that query, so
/// there is nothing to undo. Reported from the running program, where a run of
/// unmatched keys had accumulated `search: exawfwfoij`.
#[test]
fn a_miss_is_refused_and_leaves_the_last_match_standing() {
    let mut app = app_with(&["alpha", "thorin"]);
    type_text(&mut app, "tho");
    let before = cursor(&app);

    type_text(&mut app, "z");

    assert_eq!(
        buffer(&app),
        "tho",
        "the refused character is not in the buffer"
    );
    assert_eq!(cursor(&app), before, "and the cursor does not move");
    let message = app.message.clone().unwrap_or_default();
    assert!(message.contains("no match"), "{message}");

    // And a run of them accumulates nothing, which is the reported symptom.
    type_text(&mut app, "xqvw");
    assert_eq!(
        buffer(&app),
        "tho",
        "a run of misses still leaves the last match"
    );
    assert_eq!(cursor_name(&app), "thorin");
}

/// The first character typed is refused the same way, with an empty buffer.
#[test]
fn a_first_character_that_matches_nothing_starts_no_search() {
    let mut app = app_with(&["alpha", "thorin"]);
    type_text(&mut app, "z");
    assert!(buffer(&app).is_empty(), "no search was started");
    let message = app.message.clone().unwrap_or_default();
    assert!(message.contains("no match"), "{message}");
}

#[test]
fn the_buffer_clears_on_cursor_movement_and_on_leaving_the_directory() {
    let mut app = app_with(&["alpha", "thorin"]);
    type_text(&mut app, "tho");
    press(&mut app, KeyCode::Down, NONE);
    assert!(buffer(&app).is_empty(), "cursor movement clears it");

    type_text(&mut app, "tho");
    app.navigate(Side::Left, VfsPath::local("/tmp"));
    assert!(buffer(&app).is_empty(), "leaving the directory clears it");
}

// -------------------------------------------------------- the ambiguous keys

#[test]
fn backspace_pops_the_buffer_and_otherwise_goes_to_the_parent() {
    let mut app = app_with(&["alpha", "thorin"]);
    type_text(&mut app, "tho");

    press(&mut app, KeyCode::Backspace, NONE);
    assert_eq!(buffer(&app), "th", "branch one: pop and re-match");
    assert_eq!(cursor_name(&app), "thorin");
    assert!(
        app.take_pending_reads().is_empty(),
        "popping must not navigate"
    );

    press(&mut app, KeyCode::Backspace, NONE);
    press(&mut app, KeyCode::Backspace, NONE);
    assert!(buffer(&app).is_empty());
    assert!(app.take_pending_reads().is_empty());

    press(&mut app, KeyCode::Backspace, NONE);
    let reads = app.take_pending_reads();
    assert_eq!(reads.len(), 1, "branch two: go up");
    assert_eq!(
        reads.first().map(|r| r.path.clone()),
        Some(VfsPath::local("/home/thorin"))
    );
}

#[test]
fn ctrl_pgup_goes_up_even_while_a_search_is_running() {
    // `parent` is bound to both keys, but the design gives the pop rule to
    // Backspace specifically.
    let mut app = app_with(&["thorin"]);
    type_text(&mut app, "tho");
    press(&mut app, KeyCode::PageUp, CTRL);
    assert_eq!(app.take_pending_reads().len(), 1);
}

#[test]
fn space_extends_the_buffer_and_otherwise_marks() {
    let mut app = app_with(&["alpha", "my file.txt", "zeta"]);

    // Branch two first: an empty buffer means Space marks.
    press(&mut app, KeyCode::Char(' '), NONE);
    assert_eq!(
        app.left
            .active_tab()
            .marks
            .iter()
            .next()
            .map(String::as_str),
        Some("alpha")
    );
    assert!(buffer(&app).is_empty());

    // Branch one: a running search takes the space, because filenames contain
    // spaces.
    type_text(&mut app, "my");
    press(&mut app, KeyCode::Char(' '), NONE);
    assert_eq!(buffer(&app), "my ");
    type_text(&mut app, "fi");
    assert_eq!(buffer(&app), "my fi");
    assert_eq!(cursor_name(&app), "my file.txt");
    assert_eq!(app.left.active_tab().marks.len(), 1, "nothing else marked");
}

#[test]
fn insert_always_marks_whatever_the_buffer_holds() {
    let mut app = app_with(&["alpha", "thorin", "zeta"]);
    type_text(&mut app, "tho");
    assert_eq!(cursor_name(&app), "thorin");

    press(&mut app, KeyCode::Insert, NONE);
    assert!(
        app.left.active_tab().marks.contains("thorin"),
        "Insert is never part of a search"
    );
    assert_eq!(cursor_name(&app), "zeta", "and it moves down");

    // Again with no buffer at all.
    press(&mut app, KeyCode::Insert, NONE);
    assert!(app.left.active_tab().marks.contains("zeta"));
}

#[test]
fn bare_digits_feed_the_buffer_by_default() {
    // `2026-budget.xlsx` has to be reachable by its first
    // character.
    let mut app = app_with(&["alpha", "2026-budget.xlsx"]);
    app.left.open_tab(VfsPath::local("/etc"), 9);
    app.left.select_tab(0);

    type_text(&mut app, "20");
    assert_eq!(buffer(&app), "20");
    assert_eq!(cursor_name(&app), "2026-budget.xlsx");
    assert_eq!(app.left.active_index(), 0, "no tab was switched");
}

#[test]
fn digit_keys_tabs_switches_tabs_while_the_buffer_is_empty() {
    let mut config = Config::default();
    config.panel.digit_keys = DigitKeys::Tabs;
    // `a2.log` exists so the last assertion can prove a digit reached the
    // buffer. Without a name it can match, the digit is now refused - which is
    // correct, and would make this test look like it was about tabs when it is
    // about where a digit goes.
    let mut app = app_with_config(config, &["alpha", "2026-budget.xlsx", "a2.log"]);
    app.left.open_tab(VfsPath::local("/etc"), 9);
    assert_eq!(app.left.tab_count(), 2);
    app.left.select_tab(0);

    press(&mut app, KeyCode::Char('2'), NONE);
    assert_eq!(app.left.active_index(), 1, "digits switch tabs");
    assert!(buffer(&app).is_empty());

    press(&mut app, KeyCode::Char('9'), NONE);
    assert_eq!(
        app.left.active_index(),
        1,
        "no tab 9; a message, not a panic"
    );
    assert!(app.message.is_some());

    // With a buffer running, digits extend it instead.
    app.left.select_tab(0);
    type_text(&mut app, "a");
    press(&mut app, KeyCode::Char('2'), NONE);
    assert_eq!(
        buffer(&app),
        "a2",
        "the digit extended the buffer rather than switching tab"
    );
    assert_eq!(app.left.active_index(), 0);
}

#[test]
fn esc_clears_the_buffer_first_and_the_marks_second() {
    let mut app = app_with(&["alpha", "thorin"]);
    press(&mut app, KeyCode::Insert, NONE); // marks "alpha"
    type_text(&mut app, "tho");
    assert_eq!(app.left.active_tab().marks.len(), 1);

    press(&mut app, KeyCode::Esc, NONE);
    assert!(buffer(&app).is_empty(), "stage one: the buffer");
    assert_eq!(
        app.left.active_tab().marks.len(),
        1,
        "marks survive stage one"
    );

    press(&mut app, KeyCode::Esc, NONE);
    assert!(
        app.left.active_tab().marks.is_empty(),
        "stage two: the marks"
    );
}

#[test]
fn esc_ends_a_search_that_ctrl_s_armed_but_nothing_was_typed_into() {
    // the quick-search buffer "is cleared by Esc, by cursor
    // movement, or by leaving the directory". Ctrl+S makes the search visible
    // in the panel status line with an empty buffer, and Esc has to end it -
    // otherwise `search: [aa]` sits there with no key that dismisses it.
    let mut app = app_with(&["alpha", "thorin"]);
    press(&mut app, KeyCode::Char('a'), CTRL); // mark everything
    assert_eq!(app.left.active_tab().marks.len(), 2);
    press(&mut app, KeyCode::Char('s'), CTRL); // start a search explicitly
    assert!(app.left.quick.is_active(), "Ctrl+S shows a search");

    press(&mut app, KeyCode::Esc, NONE);
    assert!(
        !app.left.quick.is_active(),
        "Esc must end a search the status line is showing"
    );
    // The buffer was empty, so this Esc is also the one that clears the
    // selection (the second clause).
    assert!(app.left.active_tab().marks.is_empty());
}

#[test]
fn ctrl_w_closes_a_tab_on_a_panel_and_kills_a_word_on_the_command_line() {
    // resolved by context, not by a special case.
    let mut app = app_with(&["alpha"]);
    app.left.open_tab(VfsPath::local("/etc"), 9);
    assert_eq!(app.left.tab_count(), 2);
    press(&mut app, KeyCode::Char('w'), CTRL);
    assert_eq!(app.left.tab_count(), 1, "panel focus: close the tab");

    app.set_focus(Focus::CommandLine);
    app.cmdline.set_text("git commit --amend");
    app.cmdline.move_end();
    press(&mut app, KeyCode::Char('w'), CTRL);
    assert_eq!(
        app.cmdline.text(),
        "git commit ",
        "cmdline focus: kill a word"
    );
    assert_eq!(app.left.tab_count(), 1, "and no tab was touched");
}

#[test]
fn ctrl_h_toggles_hidden_files_only_under_the_enhanced_protocol() {
    // Enhanced: both keys work.
    let mut app = app_with(&["alpha", "thorin"]);
    app.keyboard.enhanced = true;
    let before = app.config.panel.show_hidden;
    press(&mut app, KeyCode::Char('h'), CTRL);
    assert_eq!(app.config.panel.show_hidden, !before);

    // Legacy: Backspace wins, so Ctrl+H *is* Backspace and the toggle is
    // unavailable - alt+period is the documented fallback.
    let mut app = app_with(&["alpha", "thorin"]);
    app.keyboard.enhanced = false;
    let before = app.config.panel.show_hidden;
    type_text(&mut app, "tho");
    press(&mut app, KeyCode::Char('h'), CTRL);
    assert_eq!(
        app.config.panel.show_hidden, before,
        "no toggle on a legacy terminal"
    );
    assert_eq!(buffer(&app), "th", "it popped the buffer, like Backspace");

    press(&mut app, KeyCode::Char('.'), ALT);
    assert_eq!(
        app.config.panel.show_hidden, !before,
        "alt+period still works"
    );
}

// --------------------------------------------------------- focus and the caret

#[test]
fn left_and_right_enter_the_command_line_at_the_remembered_caret() {
    let mut app = app_with(&["alpha"]);
    app.cmdline.set_text("cp  /dest");
    app.cmdline.set_caret(3);

    press(&mut app, KeyCode::Right, NONE);
    assert_eq!(app.focus, Focus::CommandLine);
    assert_eq!(app.cmdline.caret(), 3, "the caret is where it was left");
    assert_eq!(
        app.active_side,
        Side::Left,
        "the active panel does not change"
    );

    // And Left does the same thing from a panel.
    app.set_focus(Focus::Panel(Side::Left));
    press(&mut app, KeyCode::Left, NONE);
    assert_eq!(app.focus, Focus::CommandLine);
    assert_eq!(app.cmdline.caret(), 3);
}

#[test]
fn up_and_down_leave_the_command_line_with_the_text_and_caret_intact() {
    let mut app = app_with(&["alpha"]);
    press(&mut app, KeyCode::Right, NONE);
    type_text(&mut app, "cp ");
    assert_eq!(app.cmdline.text(), "cp ");
    assert_eq!(app.cmdline.caret(), 3);

    press(&mut app, KeyCode::Up, NONE);
    assert_eq!(app.focus, Focus::Panel(Side::Left));
    assert_eq!(
        app.cmdline.text(),
        "cp ",
        "a half-typed command is never lost"
    );
    assert_eq!(app.cmdline.caret(), 3);

    press(&mut app, KeyCode::Right, NONE);
    press(&mut app, KeyCode::Down, NONE);
    assert_eq!(app.focus, Focus::Panel(Side::Left), "Down leaves too");
    assert_eq!(app.cmdline.caret(), 3);
}

#[test]
fn left_and_right_move_the_caret_once_the_command_line_has_focus() {
    let mut app = app_with(&["alpha"]);
    app.cmdline.set_text("abcdef");
    app.cmdline.set_caret(6);
    app.set_focus(Focus::CommandLine);

    press(&mut app, KeyCode::Left, NONE);
    press(&mut app, KeyCode::Left, NONE);
    assert_eq!(app.cmdline.caret(), 4);
    assert_eq!(
        app.focus,
        Focus::CommandLine,
        "they never change focus here"
    );
    press(&mut app, KeyCode::Right, NONE);
    assert_eq!(app.cmdline.caret(), 5);
}

#[test]
fn the_caret_survives_a_full_focus_round_trip() {
    // the walkthrough, step by step.
    let mut app = app_with(&["alpha", "notes.txt", "zeta"]);

    press(&mut app, KeyCode::Right, NONE); // 1. into the command line
    type_text(&mut app, "cp "); //            2. caret at 3
    assert_eq!(app.cmdline.caret(), 3);

    // 3. back to the panel, text kept. Up also steps the cursor;
    //    the cursor is already on `alpha`, the first row, so it stays there.
    press(&mut app, KeyCode::Up, NONE);
    assert_eq!(app.focus, Focus::Panel(Side::Left));
    assert_eq!(app.cmdline.text(), "cp ");
    assert_eq!(app.cmdline.caret(), 3);

    press(&mut app, KeyCode::Down, NONE); //  4. move to notes.txt
    assert_eq!(cursor_name(&app), "notes.txt");
    press(&mut app, KeyCode::Enter, CTRL);
    assert_eq!(app.cmdline.text(), "cp notes.txt ");
    assert_eq!(
        app.focus,
        Focus::CommandLine,
        "Ctrl+Enter takes focus with it"
    );

    // 5. Down returns to the panel *and* steps onto the next entry, which is
    //    what makes consecutive filenames two keys each.
    press(&mut app, KeyCode::Down, NONE);
    assert_eq!(app.focus, Focus::Panel(Side::Left));
    assert_eq!(cursor_name(&app), "zeta");
    press(&mut app, KeyCode::Enter, CTRL);
    assert_eq!(app.cmdline.text(), "cp notes.txt zeta ");
    assert_eq!(app.cmdline.caret(), 18);

    // 6. already on the command line; Enter would run it.
    assert_eq!(app.focus, Focus::CommandLine);
    assert_eq!(app.cmdline.caret(), 18);
}

#[test]
fn ctrl_enter_inserts_at_a_mid_line_caret_with_a_separating_space() {
    let mut app = app_with(&["alpha", "notes.txt"]);
    app.cmdline.set_text("cp /dest");
    app.cmdline.set_caret(3);
    press(&mut app, KeyCode::Down, NONE);

    press(&mut app, KeyCode::Enter, CTRL);
    assert_eq!(
        app.cmdline.text(),
        "cp notes.txt /dest",
        "at the caret, not appended at the end"
    );
    assert_eq!(app.cmdline.caret(), 13, "just past what was inserted");

    // A space that is already there is not doubled.
    app.cmdline.set_text("cp  /dest");
    app.cmdline.set_caret(3);
    press(&mut app, KeyCode::Enter, CTRL);
    assert_eq!(app.cmdline.text(), "cp notes.txt /dest");
}

#[test]
fn ctrl_enter_works_the_same_way_from_command_line_focus() {
    let mut app = app_with(&["alpha", "notes.txt"]);
    press(&mut app, KeyCode::Down, NONE);
    press(&mut app, KeyCode::Right, NONE);
    type_text(&mut app, "cp ");
    press(&mut app, KeyCode::Enter, CTRL);
    assert_eq!(app.cmdline.text(), "cp notes.txt ");
    assert_eq!(
        app.focus,
        Focus::CommandLine,
        "it never changes focus, in either direction"
    );
}

#[test]
fn ctrl_shift_enter_inserts_the_full_path() {
    let mut app = app_with(&["notes.txt"]);
    press(&mut app, KeyCode::Enter, CTRL | SHIFT);
    assert_eq!(app.cmdline.text(), "/home/thorin/src/notes.txt ");
    // Same focus rule as Ctrl+Enter.
    assert_eq!(app.focus, Focus::CommandLine);
}

#[test]
fn inserted_names_are_shell_quoted_when_they_need_it() {
    // the command line runs through a shell, so an unquoted
    // "My Report (final).pdf" is a bug.
    let mut app = app_with(&["My Report (final).pdf", "it's here.txt", "plain.txt"]);
    app.cmdline.set_text("cat ");
    app.cmdline.move_end();

    press(&mut app, KeyCode::Enter, CTRL);
    assert_eq!(app.cmdline.text(), "cat 'My Report (final).pdf' ");

    press(&mut app, KeyCode::Down, NONE);
    press(&mut app, KeyCode::Enter, CTRL);
    assert_eq!(
        app.cmdline.text(),
        "cat 'My Report (final).pdf' 'it'\\''s here.txt' ",
        "a single quote closes, escapes and reopens"
    );

    press(&mut app, KeyCode::Down, NONE);
    press(&mut app, KeyCode::Enter, CTRL);
    assert!(
        app.cmdline.text().ends_with("plain.txt "),
        "a name that needs no quoting stays readable: {}",
        app.cmdline.text()
    );
}

#[test]
fn a_multibyte_filename_inserts_at_a_caret_without_corrupting_the_line() {
    let mut app = app_with(&["héllo 𠀀.mp3"]);
    app.cmdline.set_text("mpv --loop");
    app.cmdline.set_caret(4); // just after "mpv "

    press(&mut app, KeyCode::Enter, CTRL);
    assert_eq!(app.cmdline.text(), "mpv 'héllo 𠀀.mp3' --loop");
    assert_eq!(
        app.cmdline.caret(),
        18,
        "a character index: 4 + 13 quoted characters + the separating space"
    );
    // The caret still lands on a character boundary, so the renderer can slice.
    assert!(
        app.cmdline
            .text()
            .is_char_boundary(app.cmdline.byte_offset())
    );
}

// ------------------------------------------------------ command-line editing

#[test]
fn printable_keys_insert_at_the_caret_with_the_command_line_focused() {
    let mut app = app_with(&["alpha"]);
    app.set_focus(Focus::CommandLine);
    type_text(&mut app, "ls -la");
    assert_eq!(app.cmdline.text(), "ls -la");
    app.cmdline.set_caret(2);
    type_text(&mut app, " -x");
    assert_eq!(app.cmdline.text(), "ls -x -la");
    assert!(
        app.left.quick.is_empty(),
        "nothing reached the quick search"
    );
}

#[test]
fn panel_keys_are_literal_characters_on_the_command_line() {
    // every panel key applies only while the panel has focus.
    let mut app = app_with(&["alpha", "beta"]);
    app.set_focus(Focus::CommandLine);
    type_text(&mut app, "rm ");
    press(&mut app, KeyCode::Char('*'), NONE);
    press(&mut app, KeyCode::Char('+'), NONE);
    press(&mut app, KeyCode::Char('-'), NONE);
    press(&mut app, KeyCode::Char(' '), NONE);
    assert_eq!(app.cmdline.text(), "rm *+- ");
    assert!(app.left.active_tab().marks.is_empty(), "nothing marks");

    // Insert toggles overwrite mode instead of marking.
    assert!(!app.cmdline.overwrite);
    press(&mut app, KeyCode::Insert, NONE);
    assert!(app.cmdline.overwrite);
    assert!(app.left.active_tab().marks.is_empty());
}

#[test]
fn readline_editing_keys_work_on_the_command_line() {
    let mut app = app_with(&["alpha"]);
    app.set_focus(Focus::CommandLine);
    type_text(&mut app, "git commit --amend");

    press(&mut app, KeyCode::Char('a'), CTRL);
    assert_eq!(app.cmdline.caret(), 0, "ctrl+a: line start");
    press(&mut app, KeyCode::Char('e'), CTRL);
    assert_eq!(app.cmdline.caret(), 18, "ctrl+e: line end");
    press(&mut app, KeyCode::Char('w'), CTRL);
    assert_eq!(app.cmdline.text(), "git commit ");
    app.cmdline.set_caret(4);
    press(&mut app, KeyCode::Char('k'), CTRL);
    assert_eq!(app.cmdline.text(), "git ", "ctrl+k: kill to end");
    press(&mut app, KeyCode::Char('u'), CTRL);
    assert_eq!(app.cmdline.text(), "", "ctrl+u: kill the line");
    assert_eq!(app.cmdline.caret(), 0);
}

#[test]
fn esc_on_the_command_line_clears_then_returns_to_the_panel() {
    let mut app = app_with(&["alpha"]);
    app.set_focus(Focus::CommandLine);
    type_text(&mut app, "rm -rf /");
    press(&mut app, KeyCode::Esc, NONE);
    assert_eq!(app.cmdline.text(), "");
    assert_eq!(app.cmdline.caret(), 0, "clearing is what resets the caret");
    assert_eq!(app.focus, Focus::CommandLine, "still here");
    press(&mut app, KeyCode::Esc, NONE);
    assert_eq!(app.focus, Focus::Panel(Side::Left));
}

#[test]
fn enter_on_the_command_line_keeps_the_command_and_says_where_it_went() {
    // A headless App has no PTY, which the design keeps as a state that has
    // to work: this application owns the text and the caret, Enter takes the
    // line, and the status line says why nothing ran rather than pretending
    // something did.
    let mut app = app_with(&["alpha"]);
    app.set_focus(Focus::CommandLine);
    type_text(&mut app, "make test");
    press(&mut app, KeyCode::Enter, NONE);

    assert_eq!(app.cmdline.text(), "");
    assert_eq!(
        app.cmdline.caret(),
        0,
        "running the command resets the caret"
    );
    assert_eq!(app.cmdline.history, vec!["make test".to_string()]);
    let message = app.message.clone().unwrap_or_default();
    assert!(message.contains("no shell is running"), "{message}");

    // And the history walks back, with ctrl+up and with the alt+p fallback.
    press(&mut app, KeyCode::Up, CTRL);
    assert_eq!(app.cmdline.text(), "make test");
    press(&mut app, KeyCode::Down, CTRL);
    assert_eq!(app.cmdline.text(), "");
    press(&mut app, KeyCode::Char('p'), ALT);
    assert_eq!(app.cmdline.text(), "make test");
    press(&mut app, KeyCode::Char('n'), ALT);
    assert_eq!(app.cmdline.text(), "");
}

// ----------------------------------------------------------- action dispatch

#[test]
fn tab_moves_between_panels_and_alt_digits_move_between_tabs() {
    let mut app = app_with(&["alpha"]);
    press(&mut app, KeyCode::Tab, NONE);
    assert_eq!(app.focus, Focus::Panel(Side::Right));
    assert_eq!(app.active_side, Side::Right);
    press(&mut app, KeyCode::Tab, NONE);
    assert_eq!(app.focus, Focus::Panel(Side::Left));

    app.left.open_tab(VfsPath::local("/etc"), 9);
    assert_eq!(app.left.active_index(), 1);
    press(&mut app, KeyCode::Char('1'), ALT);
    assert_eq!(app.left.active_index(), 0);
    press(&mut app, KeyCode::Char('2'), ALT);
    assert_eq!(app.left.active_index(), 1);
    press(&mut app, KeyCode::Char('7'), ALT);
    assert_eq!(app.left.active_index(), 1, "no tab 7, and no panic");
    assert!(app.message.is_some());
}

#[test]
fn ctrl_digits_sort_by_the_nth_configured_column() {
    // positional over panel.columns.order, which by default is
    // name, ext, size, date, attr, git.
    let mut app = app_with(&["alpha"]);
    press(&mut app, KeyCode::Char('3'), CTRL);
    assert_eq!(
        app.left.active_tab().sort.key,
        SortKey::Column(ColumnId::Size)
    );
    assert!(!app.left.active_tab().sort.reverse);
    press(&mut app, KeyCode::Char('3'), CTRL);
    assert!(app.left.active_tab().sort.reverse, "the same key reverses");

    press(&mut app, KeyCode::Char('4'), CTRL);
    assert_eq!(
        app.left.active_tab().sort.key,
        SortKey::Column(ColumnId::Date)
    );

    // The sixth column is the git-state flag.
    press(&mut app, KeyCode::Char('6'), CTRL);
    assert_eq!(
        app.left.active_tab().sort.key,
        SortKey::Column(ColumnId::GitState)
    );

    // A position past the end of the configured order wraps to the start
    // rather than refusing. With the default six columns that makes Ctrl+7 the
    // first one again - the fallback a terminal needs when it will not deliver
    // Ctrl+1, either because it cannot encode it or because it has taken it for
    // its own tabs.
    press(&mut app, KeyCode::Char('7'), CTRL);
    assert_eq!(
        app.left.active_tab().sort.key,
        SortKey::Column(ColumnId::Name),
        "ctrl+7 is ctrl+1 again"
    );
    press(&mut app, KeyCode::Char('9'), CTRL);
    assert_eq!(
        app.left.active_tab().sort.key,
        SortKey::Column(ColumnId::Size),
        "ctrl+9 is ctrl+3 again"
    );
}

#[test]
fn a_reordered_column_layout_moves_the_sort_keys_with_it() {
    let mut config = Config::default();
    config.panel.columns.order = vec![ColumnId::Name, ColumnId::Size, ColumnId::Date];
    let mut app = app_with_config(config, &["alpha"]);
    press(&mut app, KeyCode::Char('2'), CTRL);
    assert_eq!(
        app.left.active_tab().sort.key,
        SortKey::Column(ColumnId::Size)
    );
    // And the wrap follows the layout: with three columns the fourth key is
    // the first one again.
    press(&mut app, KeyCode::Char('4'), CTRL);
    assert_eq!(
        app.left.active_tab().sort.key,
        SortKey::Column(ColumnId::Name),
        "ctrl+4 wraps to the first of three columns"
    );
}

#[test]
fn no_shipped_key_says_not_implemented_any_more() {
    // never a panic, never a silent no-op. v0.7 is
    // the last line, so there is nothing left above `CURRENT` and the
    // two keys this test used to use as examples of "not yet" - shift+F2 and
    // F9 - now do their job. Updated rather than deleted:
    // what it asserts now is that **no** key does
    // and that these two act.
    let mut app = app_with(&["alpha"]);

    // shift+F2 compares the two listings and reports what it marked.
    app.message = None;
    press(&mut app, KeyCode::F(2), SHIFT);
    let message = app.message.clone().unwrap_or_default();
    assert!(message.contains("compared"), "{message}");

    // F9 opens the menu bar, which is a dialog rather than a message.
    app.message = None;
    press(&mut app, KeyCode::F(9), NONE);
    assert!(app.dialog_is_open(), "F9 drops the menu bar");
    press(&mut app, KeyCode::Esc, NONE);
    assert!(!app.dialog_is_open(), "and Esc gives the panel back");

    assert!(!app.should_quit);

    // And nothing bound in the shipped keymap reports itself as unbuilt.
    for action in holoscommander::input::Action::ALL {
        assert!(
            action.implemented(),
            "{action} is above Milestone::CURRENT, which is now the \
             last line"
        );
    }
}

#[test]
fn a_two_key_chord_resolves_over_two_presses() {
    // Nothing shipped is a chord any more - the fallbacks are single
    // alt+letter keys and the design gives ctrl+x back to cut - so this
    // binds its own, which is what the `[terminal.sequences]` escape hatch
    // would do.
    use holoscommander::input::{Binding, KeyContext, KeyPress};

    let mut app = app_with(&["alpha"]);
    app.keymap.bind(
        Some(KeyContext::Panel),
        Binding::Chord(
            KeyPress::new(KeyCode::Char('k'), CTRL),
            KeyPress::plain(KeyCode::Char('d')),
        ),
        holoscommander::input::Action::DriveLeft,
    );

    press(&mut app, KeyCode::Char('k'), CTRL);
    assert!(
        app.keyboard.pending_chord.is_some(),
        "the first half is remembered"
    );
    let message = app.message.clone().unwrap_or_default();
    assert!(
        message.contains("ctrl+k"),
        "the status line shows it: {message}"
    );

    press(&mut app, KeyCode::Char('d'), NONE);
    assert!(app.keyboard.pending_chord.is_none());
    // It completes into the device picker, which queues the popup for the
    // event loop: enumerating mounts reads /proc/mounts and dispatch may not
    // read.
    assert_eq!(
        app.drives_pending(),
        Some(holoscommander::app::DrivesRequest::Devices(
            holoscommander::panel::Side::Left
        )),
        "the chord completes into alt+F1's request"
    );
}

#[test]
fn key_release_events_do_nothing() {
    let mut app = app_with(&["alpha", "thorin"]);
    let release = KeyEvent::new_with_kind(KeyCode::Char('t'), NONE, KeyEventKind::Release);
    dispatch(&mut app, release).expect("dispatch");
    assert!(buffer(&app).is_empty(), "only press and repeat act");
}

#[test]
fn quitting_is_a_flag_not_an_exit() {
    for (code, mods) in [
        (KeyCode::F(10), NONE),
        (KeyCode::Char('q'), ALT),
        (KeyCode::F(4), ALT),
    ] {
        let mut app = app_with(&["alpha"]);
        // `ui.confirm_exit` is on by default and puts a prompt in front of all
        // three; what is under test here is that quitting is a
        // flag rather than a process exit, so the prompt is turned off.
        app.config.ui.confirm_exit = false;
        press(&mut app, code, mods);
        assert!(app.should_quit, "{code:?} quits");
    }
}

#[test]
fn every_quit_key_goes_through_the_same_prompt() {
    // F10, Alt+Q and Alt+F4 all quit, so all three owe the
    // ui.confirm_exit prompt rather than one of them being a back door.
    for (code, mods) in [
        (KeyCode::F(10), NONE),
        (KeyCode::Char('q'), ALT),
        (KeyCode::F(4), ALT),
    ] {
        let mut app = app_with(&["alpha"]);
        app.config.ui.confirm_exit = true;
        press(&mut app, code, mods);
        assert!(!app.should_quit, "{code:?} prompts first");
        assert!(app.dialog_is_open(), "{code:?} opens the prompt");
        press(&mut app, KeyCode::Char('y'), NONE);
        assert!(app.should_quit, "{code:?} quits once answered");
    }
}

/// "F1 is context-sensitive: F1 in a dialog explains that dialog,
/// F1 in the viewer explains the viewer, F1 in the console explains the
/// console."
///
/// Queued rather than rendered, because the page is generated from the live
/// keymap and the live column order and dispatch has no business rendering
/// thirty pages of text on a keystroke.
#[test]
fn f1_asks_for_the_section_of_the_reference_it_was_pressed_in() {
    use holoscommander::app::ViewRequest;
    use holoscommander::input::DialogId;
    use holoscommander::ui::help::HelpTopic;

    let topic = |app: &mut App| match app.take_pending_view() {
        Some(ViewRequest::Help { topic }) => topic,
        other => panic!("F1 queues a help page, not {other:?}"),
    };

    // From a panel: the keyboard reference, which the design makes "its first
    // and default page".
    let mut app = app_with(&["alpha"]);
    press(&mut app, KeyCode::F(1), NONE);
    assert_eq!(topic(&mut app), HelpTopic::Keyboard);

    // From a dialog: that dialog's paragraph, **on top of it**. It used to
    // queue a viewer, and dialogs draw over viewers, so the answer appeared
    // behind the question that prompted it.
    press(&mut app, KeyCode::Char('g'), CTRL);
    assert_eq!(app.focus, Focus::Dialog(DialogId::GotoPath));
    press(&mut app, KeyCode::F(1), NONE);
    assert!(
        app.take_pending_view().is_none(),
        "the explanation is a popup, not a viewer underneath the dialog"
    );
    assert_eq!(
        app.focus,
        Focus::Dialog(DialogId::Message),
        "and it is the thing now on top: a message box, which is what every          other multi-line answer in this program is"
    );
}

#[test]
fn dispatch_touches_neither_the_terminal_nor_the_filesystem() {
    // Every navigation is a queued request; the event loop services it.
    let mut app = app_with(&["alpha"]);
    press(&mut app, KeyCode::Char('\\'), CTRL); // root
    assert_eq!(app.take_pending_reads().len(), 1);
    press(&mut app, KeyCode::Char('r'), CTRL | ALT); // reload config
    assert!(app.reload_requested, "a flag, not a config read");
}

#[test]
fn the_status_message_belongs_to_the_key_that_produced_it() {
    // The secondary sort rather than alt+F1: the device picker queues a popup
    // now, and a queued popup is not a status message. What is being tested is
    // the clearing rule, so any key that reports will do.
    //
    let mut app = app_with(&["alpha"]);
    press(&mut app, KeyCode::Char('1'), CTRL | SHIFT);
    assert!(app.message.is_some(), "ctrl+shift+1 says what it sorted by");
    press(&mut app, KeyCode::Down, NONE);
    assert!(app.message.is_none(), "the next key clears it");
}

#[test]
fn the_tab_bar_setting_is_readable_without_a_terminal() {
    // Guards the headless contract the renderer relies on: nothing in the
    // input path needs a frame to have been drawn.
    let app = app_with(&["alpha"]);
    assert!(!app.left.tab_bar_visible(TabBar::Auto));
    assert!(app.left.tab_bar_visible(TabBar::Always));
}

// --------------------------------------------------- going up a directory ---
// Not a the design rule: `Backspace` to the parent is specified there, but
// where the cursor lands is not. Total Commander leaves it on the directory you
// came out of, which is what makes walking back up a tree keep its place, and
// that is the behaviour these pin.

/// Drive a real streaming read of `names` into the left panel, the way the
/// event loop does: take the queued request, feed a batch, then `Done`.
fn deliver(app: &mut App, names: &[&str], dirs: &[&str]) {
    let reads = app.take_pending_reads();
    let req = reads.last().expect("a navigation queues a read");
    let (side, tab, generation) = (req.side, req.tab, req.generation);
    let mut batch: Vec<Entry> = vec![Entry::parent_entry()];
    batch.extend(dirs.iter().map(|n| Entry::dir(*n)));
    batch.extend(names.iter().map(|n| Entry::file(*n)));
    app.apply_vfs_event(holoscommander::app::VfsEvent::Entries {
        side,
        tab,
        generation,
        batch,
    });
    app.apply_vfs_event(holoscommander::app::VfsEvent::Done {
        side,
        tab,
        generation,
    });
}

#[test]
fn backspace_to_the_parent_lands_on_the_directory_we_came_out_of() {
    let mut app = App::headless(Config::default(), Keymap::builtin(), Theme::blue());
    app.navigate(
        Side::Left,
        VfsPath::local("/home/thorin/src/holoscommander"),
    );
    deliver(&mut app, &["main.rs"], &[]);

    press(&mut app, KeyCode::Backspace, NONE);
    deliver(
        &mut app,
        &["notes.txt"],
        &["alpha", "holoscommander", "zeta"],
    );

    assert_eq!(
        cursor_name(&app),
        "holoscommander",
        "the cursor should be on the directory we left, not at the top"
    );
}

#[test]
fn ctrl_pgup_lands_on_the_directory_we_came_out_of_too() {
    // Same action, different key; the rule is about the action.
    let mut app = App::headless(Config::default(), Keymap::builtin(), Theme::blue());
    app.navigate(
        Side::Left,
        VfsPath::local("/home/thorin/src/holoscommander"),
    );
    deliver(&mut app, &["main.rs"], &[]);

    press(&mut app, KeyCode::PageUp, CTRL);
    deliver(
        &mut app,
        &["notes.txt"],
        &["alpha", "holoscommander", "zeta"],
    );

    assert_eq!(cursor_name(&app), "holoscommander");
}

#[test]
fn walking_several_levels_up_keeps_its_place_at_each_one() {
    let mut app = App::headless(Config::default(), Keymap::builtin(), Theme::blue());
    app.navigate(
        Side::Left,
        VfsPath::local("/home/thorin/src/holoscommander"),
    );
    deliver(&mut app, &["main.rs"], &[]);

    press(&mut app, KeyCode::Backspace, NONE);
    deliver(&mut app, &[], &["alpha", "holoscommander", "zeta"]);
    assert_eq!(cursor_name(&app), "holoscommander");

    press(&mut app, KeyCode::Backspace, NONE);
    deliver(&mut app, &[], &["Documents", "src", "tmp"]);
    assert_eq!(cursor_name(&app), "src");
}

#[test]
fn the_cursor_lands_as_soon_as_the_name_arrives_not_when_the_listing_finishes() {
    // Reads stream, so the name is resolved per batch. A slow
    // listing must not leave the cursor parked at the top until it completes.
    let mut app = App::headless(Config::default(), Keymap::builtin(), Theme::blue());
    app.navigate(
        Side::Left,
        VfsPath::local("/home/thorin/src/holoscommander"),
    );
    deliver(&mut app, &["main.rs"], &[]);

    press(&mut app, KeyCode::Backspace, NONE);
    let reads = app.take_pending_reads();
    let req = reads.last().expect("a navigation queues a read");
    let (side, tab, generation) = (req.side, req.tab, req.generation);
    app.apply_vfs_event(holoscommander::app::VfsEvent::Entries {
        side,
        tab,
        generation,
        batch: vec![
            Entry::parent_entry(),
            Entry::dir("alpha"),
            Entry::dir("holoscommander"),
        ],
    });

    assert_eq!(
        cursor_name(&app),
        "holoscommander",
        "resolved on the batch that carried it, with the read still running"
    );
    assert!(app.left.active_tab().loading, "still streaming");
}

#[test]
fn a_name_that_never_arrives_leaves_the_cursor_at_the_top() {
    // The directory was deleted under us, or `show_hidden` excludes it.
    let mut app = App::headless(Config::default(), Keymap::builtin(), Theme::blue());
    app.navigate(
        Side::Left,
        VfsPath::local("/home/thorin/src/holoscommander"),
    );
    deliver(&mut app, &["main.rs"], &[]);

    press(&mut app, KeyCode::Backspace, NONE);
    deliver(&mut app, &["notes.txt"], &["alpha", "zeta"]);

    assert_eq!(cursor(&app), 0);
    assert!(
        app.left.active_tab().pending_select.is_none(),
        "an unsatisfied request must not survive to fire against a later read"
    );
}

#[test]
fn an_ordinary_navigation_still_starts_at_the_top() {
    // Only "go to parent" carries a selection; entering a directory does not.
    let mut app = App::headless(Config::default(), Keymap::builtin(), Theme::blue());
    app.navigate(Side::Left, VfsPath::local("/home/thorin/src"));
    deliver(&mut app, &[], &["alpha", "zeta"]);

    assert_eq!(cursor(&app), 0);
    assert!(app.left.active_tab().pending_select.is_none());
}

#[test]
fn enter_on_the_parent_row_also_lands_on_the_directory_we_came_out_of() {
    // Same navigation, reached by a different key. `Enter` on `..` goes through
    // the open path rather than the `parent` action, and used to lose the hint.
    let mut app = App::headless(Config::default(), Keymap::builtin(), Theme::blue());
    app.navigate(
        Side::Left,
        VfsPath::local("/home/thorin/src/holoscommander"),
    );
    deliver(&mut app, &["main.rs"], &[]);

    // The `..` row is index 0, where the cursor already is.
    assert_eq!(cursor_name(&app), "..");
    press(&mut app, KeyCode::Enter, NONE);
    deliver(
        &mut app,
        &["notes.txt"],
        &["alpha", "holoscommander", "zeta"],
    );

    assert_eq!(cursor_name(&app), "holoscommander");
}

#[test]
fn enter_on_an_ordinary_directory_still_starts_at_the_top() {
    // Only the `..` row carries the hint; descending does not.
    let mut app = App::headless(Config::default(), Keymap::builtin(), Theme::blue());
    app.navigate(Side::Left, VfsPath::local("/home/thorin/src"));
    deliver(&mut app, &[], &["alpha", "zeta"]);

    app.left.active_tab_mut().cursor = 1; // [alpha]
    assert_eq!(cursor_name(&app), "alpha");
    press(&mut app, KeyCode::Enter, NONE);
    deliver(&mut app, &["one.rs", "two.rs"], &[]);

    assert_eq!(cursor(&app), 0);
    assert!(app.left.active_tab().pending_select.is_none());
}

#[test]
fn leaving_the_command_line_moves_the_panel_cursor_too() {
    // Up/Down hand focus back *and* move one row, so the key keeps
    // the meaning it has in the panel.
    let mut app = app_with(&["alpha", "notes.txt", "zeta"]);
    press(&mut app, KeyCode::Down, NONE); // cursor on notes.txt
    assert_eq!(cursor_name(&app), "notes.txt");

    press(&mut app, KeyCode::Right, NONE);
    assert_eq!(app.focus, Focus::CommandLine);
    press(&mut app, KeyCode::Down, NONE);
    assert_eq!(app.focus, Focus::Panel(Side::Left));
    assert_eq!(
        cursor_name(&app),
        "zeta",
        "Down left the line and stepped down"
    );

    press(&mut app, KeyCode::Right, NONE);
    press(&mut app, KeyCode::Up, NONE);
    assert_eq!(app.focus, Focus::Panel(Side::Left));
    assert_eq!(
        cursor_name(&app),
        "notes.txt",
        "Up left the line and stepped up"
    );
}

#[test]
fn ctrl_enter_then_down_walks_consecutive_files() {
    // The compose loop step 5 describes: two keys per filename.
    let mut app = app_with(&["a.txt", "b.txt", "c.txt"]);
    app.cmdline.set_text("cp ");
    app.cmdline.move_end();

    press(&mut app, KeyCode::Enter, CTRL); // a.txt, focus -> command line
    press(&mut app, KeyCode::Down, NONE); //  -> panel, cursor on b.txt
    press(&mut app, KeyCode::Enter, CTRL); // b.txt
    press(&mut app, KeyCode::Down, NONE); //  -> panel, cursor on c.txt
    press(&mut app, KeyCode::Enter, CTRL); // c.txt

    assert_eq!(app.cmdline.text(), "cp a.txt b.txt c.txt ");
    assert_eq!(app.focus, Focus::CommandLine);
}

#[test]
fn a_reread_keeps_the_rows_on_screen_until_the_replacement_arrives() {
    // The panel flashed bare background after every copy: `reread` cleared the
    // entries immediately and every frame until the first batch landed drew an
    // empty panel. The rows now survive until there is something to put in
    // their place.
    let mut app = App::headless(Config::default(), Keymap::builtin(), Theme::blue());
    app.navigate(Side::Left, VfsPath::local("/home/thorin/src"));
    deliver(&mut app, &["a.rs", "b.rs"], &[]);
    assert_eq!(
        app.left.active_tab().entries.len(),
        3,
        "`..` plus two files"
    );

    app.reread(Side::Left);
    assert_eq!(
        app.left.active_tab().entries.len(),
        3,
        "still drawn while the re-read is in flight"
    );

    // The first batch replaces rather than appends.
    deliver(&mut app, &["a.rs", "b.rs", "c.rs"], &[]);
    assert_eq!(
        app.left.active_tab().entries.len(),
        4,
        "replaced, not appended"
    );
}

#[test]
fn a_reread_that_fails_outright_does_not_leave_stale_rows() {
    // The other half: rows that describe a directory we can no longer read must
    // not sit there looking current.
    let mut app = App::headless(Config::default(), Keymap::builtin(), Theme::blue());
    app.navigate(Side::Left, VfsPath::local("/home/thorin/src"));
    deliver(&mut app, &["a.rs"], &[]);
    assert!(!app.left.active_tab().entries.is_empty());

    app.reread(Side::Left);
    let reads = app.take_pending_reads();
    let req = reads.last().expect("a re-read queues a read");
    app.apply_vfs_event(holoscommander::app::VfsEvent::Failed {
        side: req.side,
        tab: req.tab,
        generation: req.generation,
        message: "Permission denied".to_string(),
    });
    assert!(
        app.left.active_tab().entries.is_empty(),
        "stale rows dropped"
    );
}

// ---------------------------------------------------------------------------
// the design - the operation keys, driven end to end through `dispatch`.
//
// Each of these asserts on the `JobSpec` the key produced, because that is the
// contract between the input layer and the worker: `dispatch` never touches
// the filesystem, so a queued job *is* the observable
// effect of pressing the key.
// ---------------------------------------------------------------------------

/// Both panels in real directories, with `names` on the left.
fn two_panels(names: &[&str]) -> App {
    let mut app = App::headless(Config::default(), Keymap::builtin(), Theme::blue());
    app.navigate(Side::Left, VfsPath::local("/home/thorin/src"));
    app.navigate(Side::Right, VfsPath::local("/srv/media"));
    let _ = app.take_pending_reads();
    for side in [Side::Left, Side::Right] {
        let tab = app.panel_mut(side).active_tab_mut();
        tab.entries = std::iter::once(Entry::parent_entry())
            .chain(names.iter().map(|n| Entry::file(*n)))
            .collect();
        tab.loading = false;
        tab.cursor = 1;
    }
    app
}

#[test]
fn f5_copies_the_marked_files_into_the_other_panel() {
    // the target is pre-filled with the other panel's path plus
    // a file mask, and `Enter` on it starts the copy.
    let mut app = two_panels(&["a.rs", "b.rs", "notes.txt"]);
    {
        let tab = app.left.active_tab_mut();
        tab.marks.insert("a.rs".to_string());
        tab.marks.insert("b.rs".to_string());
    }
    press(&mut app, KeyCode::F(5), NONE);
    assert!(app.dialog_is_open(), "F5 opens the copy dialog");
    press(&mut app, KeyCode::Enter, NONE);
    assert!(!app.dialog_is_open());

    let jobs = app.take_pending_jobs();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].spec.kind, holoscommander::ops::JobKind::Copy);
    let mut sources: Vec<String> = jobs[0]
        .spec
        .sources
        .iter()
        .map(ToString::to_string)
        .collect();
    sources.sort();
    assert_eq!(
        sources,
        vec!["/home/thorin/src/a.rs", "/home/thorin/src/b.rs"]
    );
    assert_eq!(
        jobs[0].spec.dest.as_ref().map(ToString::to_string),
        Some("/srv/media".to_string()),
        "the `*.*` half of the target is a mask, not a directory called `*.*`"
    );
}

#[test]
fn f5_with_nothing_marked_operates_on_the_entry_under_the_cursor() {
    // the design does not say, and this is the
    // answer `Ctrl+L` and Total Commander both give.
    let mut app = two_panels(&["a.rs", "b.rs"]);
    press(&mut app, KeyCode::F(5), NONE);
    press(&mut app, KeyCode::Enter, NONE);
    let jobs = app.take_pending_jobs();
    assert_eq!(jobs.len(), 1);
    assert_eq!(
        jobs[0]
            .spec
            .sources
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        vec!["/home/thorin/src/a.rs"]
    );
}

#[test]
fn f5_on_the_parent_row_alone_refuses_rather_than_copying_the_directory_up() {
    let mut app = two_panels(&["a.rs"]);
    app.left.active_tab_mut().cursor = 0;
    press(&mut app, KeyCode::F(5), NONE);
    assert!(
        !app.dialog_is_open(),
        "no dialog for a selection of nothing"
    );
    assert!(app.take_pending_jobs().is_empty());
    assert!(app.message.is_some(), "and it says so");
}

#[test]
fn f6_is_a_move_and_an_edited_filename_stays_on_the_destination() {
    // "edit the path and the file moves, edit the filename and
    // it is renamed" - one dialog, one target field.
    let mut app = two_panels(&["a.rs"]);
    press(&mut app, KeyCode::F(6), NONE);
    // the mask half is preselected, so typing replaces it and
    // leaves the path - which is exactly how a rename is spelled here.
    type_text(&mut app, "renamed.rs");
    press(&mut app, KeyCode::Enter, NONE);
    let jobs = app.take_pending_jobs();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].spec.kind, holoscommander::ops::JobKind::Move);
    assert_eq!(
        jobs[0].spec.dest.as_ref().map(ToString::to_string),
        Some("/srv/media/renamed.rs".to_string()),
        "the name is carried through; only the worker may stat to tell a new \
         name from an existing directory"
    );
}

#[test]
fn a_renaming_target_mask_is_refused_rather_than_silently_ignored() {
    // nothing in `ops` implements a rename
    // template, and a `*.bak` that quietly copied the names unchanged is the
    // worst of the three possible answers.
    let mut app = two_panels(&["a.rs"]);
    press(&mut app, KeyCode::F(5), NONE);
    type_text(&mut app, "/srv/media/*.bak");
    press(&mut app, KeyCode::Enter, NONE);
    assert!(app.take_pending_jobs().is_empty(), "nothing was started");
    let message = app.message.clone().unwrap_or_default();
    assert!(
        message.contains("*.bak"),
        "names what it refused: {message}"
    );
}

#[test]
fn f7_creates_a_directory_under_the_panels_own_path() {
    let mut app = two_panels(&["a.rs"]);
    press(&mut app, KeyCode::F(7), NONE);
    assert!(app.dialog_is_open());
    type_text(&mut app, "photos");
    press(&mut app, KeyCode::Enter, NONE);
    let jobs = app.take_pending_jobs();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].spec.kind, holoscommander::ops::JobKind::Mkdir);
    assert_eq!(
        jobs[0].spec.dest.as_ref().map(ToString::to_string),
        Some("/home/thorin/src/photos".to_string())
    );
}

#[test]
fn f8_trashes_and_shift_f8_unlinks() {
    // "`F8` trashes, `Shift+F8` unlinks."
    for (code, mods, want_trash) in [(KeyCode::F(8), NONE, true), (KeyCode::F(8), SHIFT, false)] {
        let mut app = two_panels(&["a.rs"]);
        press(&mut app, code, mods);
        // `F8` asks the filesystem whether it has a trash before it asks the
        // user anything. `dispatch` may not touch the
        // filesystem, so the probe is queued and the event loop services it;
        // this is that call. `Shift+F8` needs no probe and is already open.
        app.service_trash_probe();
        assert!(app.dialog_is_open(), "a confirmation, never a bare delete");
        press(&mut app, KeyCode::Char('y'), NONE);
        let jobs = app.take_pending_jobs();
        assert_eq!(jobs.len(), 1);
        assert_eq!(
            jobs[0].spec.kind,
            holoscommander::ops::JobKind::Delete { trash: want_trash }
        );
        assert_eq!(
            jobs[0]
                .spec
                .sources
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            vec!["/home/thorin/src/a.rs"]
        );
    }
}

#[test]
fn esc_on_the_delete_confirmation_deletes_nothing() {
    for mods in [NONE, SHIFT] {
        let mut app = two_panels(&["a.rs"]);
        press(&mut app, KeyCode::F(8), mods);
        app.service_trash_probe();
        press(&mut app, KeyCode::Esc, NONE);
        assert!(!app.dialog_is_open());
        assert!(app.take_pending_jobs().is_empty(), "Esc is always no");
    }
}

#[test]
fn both_delete_confirmations_default_to_the_action() {
    // the design states the rule with no trash/unlink qualifier - "**The
    // confirmation's affirmative is the default button**, not `Cancel`" - and
    // the design records it as settled: "The delete confirmation defaults to
    // the **action**, not `Cancel`."
    //
    // The alternative reading, that `Shift+F8` should open on `Cancel` because
    // it is irreversible, is what this test used to assert. It made `Enter`
    // close the prompt, delete nothing and say nothing, so the "deliberate
    // answer" it bought was really a second `Shift+F8` - and it reopened a
    // decision had settled.
    for (mods, want_trash) in [(SHIFT, false), (NONE, true)] {
        let mut app = two_panels(&["a.rs"]);
        press(&mut app, KeyCode::F(8), mods);
        app.service_trash_probe();
        press(&mut app, KeyCode::Enter, NONE);
        let jobs = app.take_pending_jobs();
        assert_eq!(jobs.len(), 1, "Enter is the affirmative");
        assert_eq!(
            jobs[0].spec.kind,
            holoscommander::ops::JobKind::Delete { trash: want_trash }
        );
    }
}

#[test]
fn f2_queue_starts_the_job_in_the_background_instead_of_on_screen() {
    // "`F2 Queue` - append to the background queue instead of
    // starting now". The job is spawned either way; what changes is the view.
    //
    let mut app = two_panels(&["a.rs"]);
    press(&mut app, KeyCode::F(5), NONE);
    press(&mut app, KeyCode::F(2), NONE);
    assert!(!app.dialog_is_open(), "the copy dialog closed");
    assert_eq!(app.take_pending_jobs().len(), 1, "and the job is queued");
    assert!(app.has_background_job());
    assert!(
        app.foreground_job_status().is_none(),
        "a queued job does not take the screen"
    );
}

#[test]
fn a_copy_opens_a_progress_dialog_and_a_walk_does_not() {
    // "Starting an operation opens a progress dialog" - but
    // the design requires `Ctrl+L` to leave the panel usable, so a `Size`
    // job never goes modal.
    let mut app = two_panels(&["a.rs"]);
    press(&mut app, KeyCode::F(5), NONE);
    press(&mut app, KeyCode::Enter, NONE);
    app.sync_job_dialogs();
    assert_eq!(
        app.top_dialog().map(holoscommander::dialog::Dialog::id),
        Some(holoscommander::input::DialogId::Progress)
    );

    let mut app = two_panels(&["a.rs"]);
    app.left.active_tab_mut().entries = vec![Entry::parent_entry(), Entry::dir("tree")];
    app.left.active_tab_mut().cursor = 1;
    press(&mut app, KeyCode::Char('l'), CTRL);
    app.sync_job_dialogs();
    assert!(!app.dialog_is_open(), "a walk never takes the screen");
}

#[test]
fn esc_abandons_a_running_walk_before_it_touches_the_search_or_the_marks() {
    // "`Esc` abandons the walk leaving the bound in place."
    let mut app = two_panels(&["a.rs"]);
    app.left.active_tab_mut().entries = vec![Entry::parent_entry(), Entry::dir("tree")];
    app.left.active_tab_mut().cursor = 1;
    app.left.active_tab_mut().marks.insert("tree".to_string());
    press(&mut app, KeyCode::Char('l'), CTRL);
    assert!(app.has_running_job());

    press(&mut app, KeyCode::Esc, NONE);
    assert_eq!(
        app.left.active_tab().marks.len(),
        1,
        "the marks are what the bound is about; Esc stopped the walk, not them"
    );
    assert!(app.message.is_some(), "and says the walk was abandoned");
}

#[test]
fn the_mask_prompt_remembers_its_mask_and_its_checkbox_for_the_session() {
    // the mask is shared between `+` and `-`, and `Exclude
    // directories` is sticky.
    let mut app = two_panels(&["a.rs", "b.rs"]);
    app.left.active_tab_mut().entries = vec![
        Entry::parent_entry(),
        Entry::dir("holiday.jpg"),
        Entry::file("beach.jpg"),
        Entry::file("a.rs"),
    ];
    press(&mut app, KeyCode::Char('+'), NONE);
    assert!(app.dialog_is_open());
    // Clear the offered `*` and type a mask.
    for _ in 0..4 {
        press(&mut app, KeyCode::Backspace, NONE);
    }
    type_text(&mut app, "*.jpg");
    // Tab to the `Exclude directories` checkbox and tick it.
    press(&mut app, KeyCode::Tab, NONE);
    press(&mut app, KeyCode::Tab, NONE);
    press(&mut app, KeyCode::Char(' '), NONE);
    press(&mut app, KeyCode::Enter, NONE);

    assert_eq!(app.masks.last, "*.jpg", "remembered for the session");
    assert!(app.masks.exclude_dirs, "and so is the checkbox");
    let marks = &app.left.active_tab().marks;
    assert!(marks.contains("beach.jpg"));
    assert!(
        !marks.contains("holiday.jpg"),
        "the own example: not a directory someone named that"
    );

    // `-` offers the same mask back (one mask, not one per key).
    press(&mut app, KeyCode::Char('-'), NONE);
    press(&mut app, KeyCode::Enter, NONE);
    assert!(
        app.left.active_tab().marks.is_empty(),
        "unmarked by the same mask"
    );
}

// -------------------------------------------------------------- tab cycling ---

#[test]
fn ctrl_tab_cycles_forward_and_ctrl_shift_tab_back_wrapping_at_both_ends() {
    // cycling within the *active* panel, wrapping, because a key
    // that silently stops at the edge reads as broken.
    let mut app = app_with(&["alpha"]);
    for _ in 0..2 {
        press(&mut app, KeyCode::Char('t'), CTRL); // three tabs in all
    }
    assert_eq!(app.left.tab_count(), 3);
    assert_eq!(app.left.active_index(), 2);

    press(&mut app, KeyCode::Tab, CTRL);
    assert_eq!(app.left.active_index(), 0, "wraps past the last");
    press(&mut app, KeyCode::Tab, CTRL);
    assert_eq!(app.left.active_index(), 1);

    press(&mut app, KeyCode::Tab, CTRL | SHIFT);
    assert_eq!(app.left.active_index(), 0);
    press(&mut app, KeyCode::Tab, CTRL | SHIFT);
    assert_eq!(app.left.active_index(), 2, "wraps past the first");
}

#[test]
fn ctrl_tab_is_a_no_op_with_one_tab_and_never_switches_panels() {
    // Plain `Tab` is the panel switch and must stay that way.
    let mut app = app_with(&["alpha"]);
    assert_eq!(app.left.tab_count(), 1);
    press(&mut app, KeyCode::Tab, CTRL);
    assert_eq!(app.left.active_index(), 0);
    assert_eq!(
        app.active_side,
        Side::Left,
        "Ctrl+Tab is not a panel switch"
    );

    press(&mut app, KeyCode::Tab, NONE);
    assert_eq!(
        app.active_side,
        Side::Right,
        "plain Tab still switches panels"
    );
}

// ------------------------------------------- the design regressions --------

/// the confirmation "naming the count", and the "last chance to notice a
/// mistake": the operation acts on **the list the prompt was written from**.
///
/// The panel is not consulted again. A listing arriving while the prompt is up -
/// `App::report_finished` re-reads both panels after every background job -
/// rebuilds and re-sorts `entries` under a cursor that is a raw index, so
/// re-deriving "the entry under the cursor" at answer time can name a different
/// file. For `Shift+F8` nothing gets that file back.
#[test]
fn a_delete_removes_what_the_prompt_named_even_if_the_listing_moved() {
    let mut app = two_panels(&["alpha.txt", "zebra.txt"]);
    // Cursor on `zebra.txt`, nothing marked.
    app.left.active_tab_mut().cursor = 2;
    assert_eq!(cursor_name_of(&app), "zebra.txt");

    press(&mut app, KeyCode::F(8), SHIFT);
    assert!(app.dialog_is_open(), "the confirmation is up");

    // The listing changes underneath: `alpha.txt` is gone and a new name sorts
    // into its place, so index 2 is now a different file.
    {
        let tab = app.left.active_tab_mut();
        tab.entries = vec![
            Entry::parent_entry(),
            Entry::file("beta.txt"),
            Entry::file("zulu.txt"),
            Entry::file("zebra.txt"),
        ];
    }
    assert_eq!(cursor_name_of(&app), "zulu.txt", "the premise");

    press(&mut app, KeyCode::Char('y'), NONE);
    let jobs = app.take_pending_jobs();
    assert_eq!(jobs.len(), 1);
    assert_eq!(
        jobs[0]
            .spec
            .sources
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        vec!["/home/thorin/src/zebra.txt"],
        "the file the prompt named, not the one now under the cursor"
    );
}

/// The same rule for `F5`/`F6`, where a mis-picked source becomes a *move*'s
/// source and its original is deleted after the copy.
#[test]
fn a_copy_moves_what_the_dialog_named_even_if_the_listing_moved() {
    let mut app = two_panels(&["alpha.txt", "zebra.txt"]);
    app.left.active_tab_mut().cursor = 2;
    press(&mut app, KeyCode::F(6), NONE);
    assert!(app.dialog_is_open());

    {
        let tab = app.left.active_tab_mut();
        tab.entries = vec![
            Entry::parent_entry(),
            Entry::file("beta.txt"),
            Entry::file("zulu.txt"),
            Entry::file("zebra.txt"),
        ];
    }
    press(&mut app, KeyCode::Enter, NONE);

    let jobs = app.take_pending_jobs();
    assert_eq!(jobs.len(), 1);
    assert_eq!(
        jobs[0]
            .spec
            .sources
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        vec!["/home/thorin/src/zebra.txt"]
    );
}

/// the target field, resolved the way `Ctrl+G` resolves a path:
/// **against the panel**, never against the process's working
/// directory, which is wherever the program was launched from.
#[test]
fn a_relative_target_resolves_against_the_panel() {
    let mut app = two_panels(&["report.txt"]);
    press(&mut app, KeyCode::F(6), NONE);
    clear_field(&mut app);
    type_text(&mut app, "2024/report.txt");
    press(&mut app, KeyCode::Enter, NONE);

    let jobs = app.take_pending_jobs();
    assert_eq!(jobs.len(), 1);
    assert_eq!(
        jobs[0].spec.dest.as_ref().map(ToString::to_string),
        Some("/home/thorin/src/2024/report.txt".to_string()),
        "under the panel, not under the cwd"
    );
}

/// And the bare-name case, which is `F6`'s own "edit the filename and it is
/// renamed".
#[test]
fn a_bare_target_name_renames_inside_the_panels_directory() {
    let mut app = two_panels(&["report.txt"]);
    press(&mut app, KeyCode::F(6), NONE);
    clear_field(&mut app);
    type_text(&mut app, "notes.bak");
    press(&mut app, KeyCode::Enter, NONE);

    let jobs = app.take_pending_jobs();
    assert_eq!(jobs.len(), 1);
    assert_eq!(
        jobs[0].spec.dest.as_ref().map(ToString::to_string),
        Some("/home/thorin/src/notes.bak".to_string())
    );
}

/// "A `+ F7` button creates a new directory **for the target**."
/// With `F5` the target is the *other* panel, and the field is then pointed at
/// what was created.
#[test]
fn plus_f7_creates_the_directory_under_the_target_and_says_so_in_the_field() {
    let mut app = two_panels(&["a.rs"]);
    press(&mut app, KeyCode::F(5), NONE);
    press(&mut app, KeyCode::F(7), NONE);
    type_text(&mut app, "2026");
    press(&mut app, KeyCode::Enter, NONE);

    let jobs = app.take_pending_jobs();
    assert_eq!(jobs.len(), 1);
    assert_eq!(
        jobs[0].spec.dest.as_ref().map(ToString::to_string),
        Some("/srv/media/2026".to_string()),
        "under the target panel, not under the source"
    );
    assert!(
        app.left.active_tab().pending_select.is_none(),
        "and the source panel's cursor is left alone"
    );

    // The field now names it, so `OK` copies into what was just created.
    press(&mut app, KeyCode::Enter, NONE);
    let jobs = app.take_pending_jobs();
    assert_eq!(jobs.len(), 1);
    assert_eq!(
        jobs[0].spec.dest.as_ref().map(ToString::to_string),
        Some("/srv/media/2026".to_string())
    );
}

/// `F2` is pure rename - filename only, stem preselected, and
/// it cannot move anything.
#[test]
fn f2_renames_in_place() {
    let mut app = two_panels(&["archive.tar.gz"]);
    press(&mut app, KeyCode::F(2), NONE);
    assert!(
        app.dialog_is_open(),
        "a rename dialog, not a status message"
    );
    assert_eq!(
        app.top_dialog().map(|d| d.title()),
        Some("Rename".to_string())
    );

    // The stem is what typing replaces; the extension stays.
    type_text(&mut app, "backup");
    press(&mut app, KeyCode::Enter, NONE);

    let jobs = app.take_pending_jobs();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].spec.kind, holoscommander::ops::JobKind::Move);
    assert_eq!(
        jobs[0].spec.dest.as_ref().map(ToString::to_string),
        Some("/home/thorin/src/backup.gz".to_string())
    );
    assert_eq!(
        jobs[0]
            .spec
            .sources
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        vec!["/home/thorin/src/archive.tar.gz"]
    );
}

/// "a second `F5` while a copy is running enqueues rather than
/// refusing." Enqueuing is work, not a display flag - the second job waits for
/// a slot rather than running beside the first.
#[test]
fn a_second_copy_waits_for_the_first_rather_than_running_beside_it() {
    let mut app = two_panels(&["a.rs"]);
    press(&mut app, KeyCode::F(5), NONE);
    press(&mut app, KeyCode::Enter, NONE);
    let first = app.take_pending_jobs();
    assert_eq!(first.len(), 1, "the first one starts");

    press(&mut app, KeyCode::F(5), NONE);
    press(&mut app, KeyCode::Enter, NONE);
    assert!(
        app.take_pending_jobs().is_empty(),
        "the second waits: two copies over one disk finish later than two in turn"
    );
    assert_eq!(app.queued_jobs(), 1);

    // When the first finishes, the second is handed out.
    finish_job(&mut app, first[0].id);
    let second = app.take_pending_jobs();
    assert_eq!(second.len(), 1, "and then it runs");
    assert_eq!(app.queued_jobs(), 0);
}

/// the summary "with the option to retry the failures".
#[test]
fn retrying_a_summary_starts_the_same_job_over_the_paths_that_failed() {
    let mut app = two_panels(&["a.rs", "b.rs"]);
    {
        let tab = app.left.active_tab_mut();
        tab.marks.insert("a.rs".to_string());
        tab.marks.insert("b.rs".to_string());
    }
    press(&mut app, KeyCode::F(5), NONE);
    press(&mut app, KeyCode::Enter, NONE);
    let started = app.take_pending_jobs();
    assert_eq!(started.len(), 1);
    let id = started[0].id;

    // One of the two failed.
    app.apply_job_event(holoscommander::ops::JobUpdate {
        id,
        event: holoscommander::ops::JobEvent::Finished {
            summary: Box::new(summary_with_failure("/home/thorin/src/b.rs")),
        },
    });
    holoscommander::input::dialog_accepted(
        &mut app,
        holoscommander::input::DialogId::JobSummary,
        holoscommander::dialog::DialogResult::Job(holoscommander::ops::JobAction::Retry(id)),
    );

    let retried = app.take_pending_jobs();
    assert_eq!(retried.len(), 1, "{:?}", app.message);
    assert_eq!(
        retried[0]
            .spec
            .sources
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        vec!["/home/thorin/src/b.rs"],
        "only what failed"
    );
    assert_eq!(
        retried[0].spec.dest.as_ref().map(ToString::to_string),
        Some("/srv/media".to_string()),
        "and the destination it was headed for"
    );
}

/// a dialog consumes all input - including the input that was
/// half-typed. An armed chord must not survive a dialog and eat the first key
/// after it closes.
#[test]
fn a_dialog_disarms_a_half_typed_chord() {
    let mut app = app_with(&["docs", "notes"]);
    let mut keymap = Keymap::builtin();
    keymap.bind(
        Some(holoscommander::input::KeyContext::Panel),
        holoscommander::input::Binding::parse("ctrl+x q").expect("a chord is a binding form"),
        holoscommander::input::Action::JobQueue,
    );
    app.keymap = keymap;

    press(&mut app, KeyCode::Char('x'), CTRL);
    assert!(
        app.message
            .as_deref()
            .unwrap_or_default()
            .contains("ctrl+x"),
        "the chord is armed: {:?}",
        app.message
    );

    // A job finishing raises a dialog with no keystroke involved at all.
    app.show_message("Done", vec!["copied 1 file".to_string()]);
    press(&mut app, KeyCode::Esc, NONE);
    assert!(!app.dialog_is_open());

    // The next key is the panel's again.
    press(&mut app, KeyCode::Char('d'), NONE);
    assert_eq!(app.left.quick.buffer, "d", "{:?}", app.message);
}

/// The name under the cursor of the left panel.
fn cursor_name_of(app: &App) -> String {
    app.left
        .active_tab()
        .current()
        .map(|e| e.name.clone())
        .unwrap_or_default()
}

/// Empty the focused dialog field.
fn clear_field(app: &mut App) {
    for _ in 0..80 {
        press(app, KeyCode::Backspace, NONE);
    }
}

/// Report a job finished with one failure against `path`.
fn summary_with_failure(path: &str) -> holoscommander::ops::JobSummary {
    holoscommander::ops::JobSummary {
        kind: holoscommander::ops::JobKind::Copy,
        files_done: 1,
        dirs_done: 0,
        bytes_done: 1,
        skipped: 0,
        failures: vec![holoscommander::ops::JobFailure {
            path: VfsPath::local(path),
            error: "No space left on device".to_string(),
        }],
        cancelled: false,
        elapsed: std::time::Duration::ZERO,
        sized: Vec::new(),
        differing: Vec::new(),
        first_difference: None,
    }
}

/// Report a job finished cleanly, the way the event loop would.
fn finish_job(app: &mut App, id: holoscommander::ops::JobId) {
    app.apply_job_event(holoscommander::ops::JobUpdate {
        id,
        event: holoscommander::ops::JobEvent::Finished {
            summary: Box::new(holoscommander::ops::JobSummary {
                kind: holoscommander::ops::JobKind::Copy,
                files_done: 1,
                dirs_done: 0,
                bytes_done: 1,
                skipped: 0,
                failures: Vec::new(),
                cancelled: false,
                elapsed: std::time::Duration::ZERO,
                sized: Vec::new(),
                differing: Vec::new(),
                first_difference: None,
            }),
        },
    });
}

/// a conflict answer goes to **the job the dialog was
/// built for**.
///
/// the design lets a backgrounded job sit parked on a question with no dialog
/// on screen while a second job asks in the foreground. Answering "whichever
/// job is parked first" then overwrites a file that was never named, and an
/// "apply to all" installs a standing policy in a batch the user was not
/// looking at.
#[test]
fn a_conflict_answer_goes_to_the_job_that_asked_it() {
    let mut app = two_panels(&["a.rs"]);
    let one = app.request_job(copy_spec("/home/thorin/src/big", "/backup"));
    let two = app.request_job(copy_spec("/home/thorin/src/small", "/backup"));
    let _ = app.take_pending_jobs();

    // Job one is backgrounded and parked: it marks itself as waiting in
    // the queue view rather than raising a dialog.
    app.background_job(one);
    park_on(&mut app, one, "/backup/archive.zip");
    park_on(&mut app, two, "/backup/notes.txt");

    app.sync_job_dialogs();
    assert_eq!(
        app.top_dialog().map(|d| d.id()),
        Some(holoscommander::input::DialogId::Conflict),
        "the foreground job's question is the one on screen"
    );

    press(&mut app, KeyCode::Char('o'), NONE);

    assert!(
        app.job(one)
            .and_then(|j| j.pending_decision.as_ref())
            .is_some(),
        "the backgrounded job was not answered on the user's behalf"
    );
    assert!(
        app.job(two)
            .and_then(|j| j.pending_decision.as_ref())
            .is_none(),
        "and the job that asked was"
    );
}

/// A copy of one path into one destination.
fn copy_spec(source: &str, dest: &str) -> holoscommander::ops::JobSpec {
    holoscommander::ops::JobSpec::new(
        holoscommander::ops::JobKind::Copy,
        vec![VfsPath::local(source)],
        Some(VfsPath::local(dest)),
    )
}

/// Park a job on a conflict about `dest`, the way its worker would.
fn park_on(app: &mut App, id: holoscommander::ops::JobId, dest: &str) {
    app.apply_job_event(holoscommander::ops::JobUpdate {
        id,
        event: holoscommander::ops::JobEvent::NeedsDecision {
            request: Box::new(holoscommander::ops::ConflictRequest {
                source: VfsPath::local("/home/thorin/src/x"),
                dest: VfsPath::local(dest),
                source_size: 1,
                dest_size: 2,
                source_mtime: None,
                dest_mtime: None,
                both_dirs: false,
                dest_is_dir: false,
            }),
        },
    });
}

// ------------------------------------------------------------- clipboard ---

#[test]
fn ctrl_c_remembers_the_entry_under_the_cursor_and_ctrl_x_marks_it_a_move() {
    // the cursor's entry, not the marks - `F5` is the operation
    // on a marked set, this is the one where the target comes after the source.
    let mut app = app_with(&["a.txt", "b.txt"]);
    press(&mut app, KeyCode::Down, NONE); // onto a.txt (index 0 is `..`-free here)
    press(&mut app, KeyCode::Char('c'), CTRL);
    let held = app.clipboard.clone().expect("something on the clipboard");
    assert!(!held.cut);
    assert_eq!(held.paths.len(), 1);
    assert!(held.describe().starts_with("copied: "));

    press(&mut app, KeyCode::Char('x'), CTRL);
    let held = app.clipboard.clone().expect("still something");
    assert!(held.cut, "Ctrl+X marks it a move");
    assert!(held.describe().starts_with("cut: "));
}

#[test]
fn the_clipboard_survives_navigation_and_panel_switches() {
    // It holds paths, not bytes, and lasts the session.
    let mut app = app_with(&["a.txt"]);
    press(&mut app, KeyCode::Char('c'), CTRL);
    assert!(app.clipboard.is_some());

    press(&mut app, KeyCode::Tab, NONE);
    assert_eq!(app.active_side, Side::Right);
    assert!(app.clipboard.is_some(), "survives a panel switch");

    press(&mut app, KeyCode::Char('t'), CTRL); // a new tab
    assert!(app.clipboard.is_some(), "survives a tab switch");
}

#[test]
fn pasting_with_an_empty_clipboard_says_so_rather_than_doing_nothing() {
    let mut app = app_with(&["a.txt"]);
    press(&mut app, KeyCode::Char('v'), CTRL);
    assert_eq!(
        app.message.as_deref(),
        Some("nothing on the clipboard"),
        "never a silent no-op (the design scope note)"
    );
}

#[test]
fn a_cut_pasted_where_it_already_is_is_refused_rather_than_renamed() {
    // moving something where it already is means nothing, and
    // quietly making a numbered copy of it would be a surprise.
    let mut app = App::headless(Config::default(), Keymap::builtin(), Theme::blue());
    app.navigate(Side::Left, VfsPath::local("/home/thorin/src"));
    deliver(&mut app, &["a.txt"], &[]);
    app.left.active_tab_mut().cursor = 1; // a.txt, past `..`

    press(&mut app, KeyCode::Char('x'), CTRL);
    press(&mut app, KeyCode::Char('v'), CTRL);
    assert_eq!(
        app.message.as_deref(),
        Some("already here; nothing to move")
    );
    assert!(app.clipboard.is_some(), "and the clipboard is not spent");
}

#[test]
fn the_parent_row_is_not_something_to_copy() {
    let mut app = App::headless(Config::default(), Keymap::builtin(), Theme::blue());
    app.navigate(Side::Left, VfsPath::local("/home/thorin/src"));
    deliver(&mut app, &["a.txt"], &[]);
    assert_eq!(cursor_name(&app), "..");
    press(&mut app, KeyCode::Char('c'), CTRL);
    assert!(app.clipboard.is_none());
    assert!(app.message.is_some(), "and it says why");
}

// ---------------------------------------------------------------------------
// the design: Focus::Viewer
// ---------------------------------------------------------------------------

/// Put the app into `Focus::Viewer` over generated text, the way the event loop
/// does it - `dispatch` may not touch the filesystem, so `F3` queues a request
/// and this is the servicing half.
fn open_viewer(app: &mut App, body: &str) {
    use holoscommander::viewer::Viewer;
    let id = app.next_viewer_id();
    let cfg = app.config.viewer.clone();
    let v = Viewer::open_memory(id, "sample.txt", body.to_string(), &cfg).expect("open");
    app.push_viewer(v);
}

#[test]
fn f3_queues_a_viewer_rather_than_opening_one() {
    // `dispatch` never reads the filesystem, so `F3`
    // records what to open and the event loop opens it - exactly as a directory
    // read is a `ReadRequest`.
    use holoscommander::app::ViewRequest;
    let mut app = app_with(&["alpha", "beta"]);
    press(&mut app, KeyCode::F(3), NONE);
    match app.pending_view() {
        Some(ViewRequest::File { path, .. }) => {
            assert_eq!(path.to_string(), "/home/thorin/src/alpha");
        }
        other => panic!("expected a queued file, got {other:?}"),
    }
    assert_eq!(app.focus, Focus::Panel(Side::Left), "focus moves on open");
    assert_eq!(app.viewer_depth(), 0);
}

#[test]
fn f3_on_a_directory_refuses_rather_than_viewing_bytes() {
    let mut app = app_with(&["alpha"]);
    app.left.active_tab_mut().entries = vec![Entry::dir("subdir")];
    app.left.active_tab_mut().cursor = 0;
    press(&mut app, KeyCode::F(3), NONE);
    assert!(app.pending_view().is_none());
    assert!(
        app.message
            .as_deref()
            .unwrap_or_default()
            .contains("directory"),
        "{:?}",
        app.message
    );
}

#[test]
fn the_viewer_consumes_all_input() {
    // "Focus::Viewer  F3; consumes all input." Nothing falls
    // through to the panel, because no panel is on screen.
    let mut app = app_with(&["alpha", "beta"]);
    open_viewer(&mut app, "one\ntwo\nthree\n");
    assert_eq!(app.focus, Focus::Viewer);

    let before = cursor(&app);
    // `F5` copies files at a panel and does nothing at a viewer.
    press(&mut app, KeyCode::F(5), NONE);
    assert_eq!(cursor(&app), before);
    assert!(
        !app.dialog_is_open(),
        "no copy dialog opened over the viewer"
    );
    // Typing does not reach the quick-search buffer. The letters that are
    // viewer keys do their own job on the way past - `e` flips the byte order
    // and `t` opens the template picker - and that is the point rather than an
    // exception: they are consumed here and never by the panel behind.
    type_text(&mut app, "beta");
    assert_eq!(cursor(&app), before);
    assert!(app.left.quick.buffer.is_empty());
    // Put back whatever one of those letters opened, so the next assertion is
    // about `F10` and not about a dialog left standing.
    if app.dialog_is_open() {
        press(&mut app, KeyCode::Esc, NONE);
    }
    assert!(!app.dialog_is_open());
    // `F10` does not quit out from under it.
    press(&mut app, KeyCode::F(10), NONE);
    assert!(!app.should_quit);
    assert!(!app.dialog_is_open());
    assert_eq!(app.focus, Focus::Viewer);
}

#[test]
fn the_viewer_keys_of_spec_10_8_all_do_something() {
    use holoscommander::config::ViewerMode;
    let mut app = app_with(&["alpha"]);
    open_viewer(&mut app, "one\ntwo\nthree\nfour\nfive\n");
    app.service_viewer(3, 40);

    // `2` / `1` / `F4` - the modes.
    press(&mut app, KeyCode::Char('2'), NONE);
    assert_eq!(app.viewer().map(|v| v.mode()), Some(ViewerMode::Hex));
    press(&mut app, KeyCode::Char('1'), NONE);
    assert_eq!(app.viewer().map(|v| v.mode()), Some(ViewerMode::Text));
    press(&mut app, KeyCode::F(4), NONE);
    assert_eq!(app.viewer().map(|v| v.mode()), Some(ViewerMode::Hex));
    press(&mut app, KeyCode::F(4), NONE);
    assert_eq!(app.viewer().map(|v| v.mode()), Some(ViewerMode::Text));

    // `w` - wrap (the design; F2 is the viewer's reload).
    assert_eq!(
        app.viewer().map(holoscommander::viewer::Viewer::wrap),
        Some(false)
    );
    press(&mut app, KeyCode::Char('w'), NONE);
    assert_eq!(
        app.viewer().map(holoscommander::viewer::Viewer::wrap),
        Some(true)
    );

    // `F8` - the encoding shortlist.
    let before = app.viewer().map(|v| v.encoding().label());
    press(&mut app, KeyCode::F(8), NONE);
    assert_ne!(app.viewer().map(|v| v.encoding().label()), before);

    // Arrows, PgDn, Home, End. the design gave the viewer a cursor and the
    // arrows move *it*; "the view follows when it reaches an edge", so one
    // `Down` on a three-row window leaves the top where it was and moves the
    // cursor to the next line. v0.4 scrolled on every press, which is what this
    // assertion used to record.
    press(&mut app, KeyCode::Down, NONE);
    assert_eq!(
        app.viewer().map(holoscommander::viewer::Viewer::top),
        Some(0),
        "the window has not moved yet"
    );
    assert_eq!(
        app.viewer().map(holoscommander::viewer::Viewer::cursor),
        Some(4),
        "and the cursor is on the second line"
    );
    press(&mut app, KeyCode::Up, NONE);
    assert_eq!(
        app.viewer().map(holoscommander::viewer::Viewer::cursor),
        Some(0)
    );
    assert_eq!(
        app.viewer().map(holoscommander::viewer::Viewer::top),
        Some(0)
    );
    // Held past the bottom row, the window follows.
    for _ in 0..4 {
        press(&mut app, KeyCode::Down, NONE);
        app.service_viewer(3, 40);
    }
    assert!(
        app.viewer().is_some_and(|v| v.top() > 0),
        "the view follows at the edge"
    );
    press(&mut app, KeyCode::Home, CTRL);
    app.service_viewer(3, 40);
    press(&mut app, KeyCode::PageDown, NONE);
    assert!(app.viewer().is_some_and(|v| v.top() > 0));
    // the file's first and last page are `Ctrl`ed, and bare
    // `Home`/`End` are the line's edges - which move the cursor within the
    // window rather than the window itself.
    press(&mut app, KeyCode::Home, CTRL);
    assert_eq!(
        app.viewer().map(holoscommander::viewer::Viewer::top),
        Some(0)
    );
    press(&mut app, KeyCode::End, CTRL);
    assert!(app.viewer().is_some_and(|v| v.top() > 0));

    // And the bare pair does not move the window at all.
    press(&mut app, KeyCode::Home, CTRL);
    let top = app.viewer().map(holoscommander::viewer::Viewer::top);
    press(&mut app, KeyCode::End, NONE);
    assert_eq!(
        app.viewer().map(holoscommander::viewer::Viewer::top),
        top,
        "bare End is the end of the line, not of the file"
    );
    press(&mut app, KeyCode::Home, NONE);
    assert_eq!(app.viewer().map(holoscommander::viewer::Viewer::top), top);
}

/// the key table, driven through `dispatch`: what starts a
/// selection, what extends it, what converts it, and what clears it
/// (the design, invariant 8).
#[test]
fn shift_extends_and_a_bare_arrow_clears() {
    use holoscommander::viewer::select::SelectKind;
    let mut app = app_with(&["alpha"]);
    // One long first line, so five presses of `Right` are five bytes rather
    // than five characters that step over a line break on the way - which they
    // would also be, and the byte count in the status line is what says how
    // many.
    open_viewer(&mut app, "abcdefghij\nsecond line\nthird line\n");
    app.service_viewer(4, 40);

    assert_eq!(app.viewer().and_then(|v| v.selection()), None);
    // `Shift+Right` five times: five bytes, anchored where the cursor was.
    for _ in 0..5 {
        press(&mut app, KeyCode::Right, SHIFT);
    }
    let sel = app
        .viewer()
        .and_then(|v| v.selection())
        .expect("Shift started a selection");
    assert_eq!(sel.range(), (0, 5), "anchored where the cursor was");
    assert_eq!(sel.kind, SelectKind::Linear);

    // `Ctrl+Shift` converts the same selection rather than starting a second.
    press(&mut app, KeyCode::Down, CTRL | SHIFT);
    let block = app
        .viewer()
        .and_then(|v| v.selection())
        .expect("still live");
    assert_eq!(block.kind, SelectKind::Rectangular);
    assert_eq!(block.anchor, sel.anchor, "the anchor did not move");

    // `Alt+B` flips it back, anchor and head unmoved.
    press(&mut app, KeyCode::Char('b'), ALT);
    let flipped = app
        .viewer()
        .and_then(|v| v.selection())
        .expect("still live");
    assert_eq!(flipped.kind, SelectKind::Linear);
    assert_eq!((flipped.anchor, flipped.head), (block.anchor, block.head));

    // A bare movement clears it.
    press(&mut app, KeyCode::Right, NONE);
    assert_eq!(
        app.viewer().and_then(|v| v.selection()),
        None,
        "a bare arrow lets the selection go"
    );
}

#[test]
fn esc_clears_the_selection_before_it_closes_the_viewer() {
    // "`Esc` clears the selection; **if there is none, close the
    // viewer**" - the key that means "never mind" undoes the smallest thing
    // first.
    let mut app = app_with(&["alpha"]);
    open_viewer(&mut app, "one\ntwo\nthree\n");
    app.service_viewer(3, 40);
    press(&mut app, KeyCode::Right, SHIFT);
    assert!(app.viewer().and_then(|v| v.selection()).is_some());

    press(&mut app, KeyCode::Esc, NONE);
    assert_eq!(app.viewer_depth(), 1, "the viewer is still up");
    assert_eq!(app.viewer().and_then(|v| v.selection()), None);
    assert_eq!(app.message.as_deref(), Some("selection cleared"));

    press(&mut app, KeyCode::Esc, NONE);
    assert_eq!(app.viewer_depth(), 0, "and the second press closes it");
}

#[test]
fn esc_closes_the_viewer_when_the_selection_has_been_shrunk_away() {
    // "`Esc` clears the selection; if there is none, close the
    // viewer." `Shift+Right` then `Shift+Left` leaves the anchor live so that
    // carrying on selects the other way round, but it covers no byte: nothing
    // is painted, the status line announces nothing and `Ctrl+C` already
    // refuses it, so `Esc` must not cost a press for it.
    let mut app = app_with(&["alpha"]);
    open_viewer(&mut app, "one\ntwo\nthree\n");
    app.service_viewer(3, 40);
    press(&mut app, KeyCode::Right, SHIFT);
    press(&mut app, KeyCode::Left, SHIFT);
    assert!(
        app.viewer()
            .and_then(|v| v.selection())
            .is_some_and(|s| s.is_empty()),
        "the anchor is still live"
    );

    press(&mut app, KeyCode::Esc, NONE);
    assert_eq!(
        app.viewer_depth(),
        0,
        "one press closes it: there was nothing to clear"
    );
}

#[test]
fn a_viewer_is_laid_out_before_the_keys_held_while_it_opened_are_applied() {
    // A file slow enough to open - an archive member read back through a
    // compressed stream - holds the keys pressed while it opens, and the event
    // loop replays them as soon as it is pushed. A viewer that has never been
    // laid out believes the screen is zero rows high, so `PgDn` would page by
    // one row and `Down` would scroll the window instead of moving the cursor
    // down it. The size the last
    // frame measured is what it is laid out at.
    let body: String = (0..400).map(|n| format!("line {n:03}\n")).collect();
    let mut app = app_with(&["alpha"]);
    // What the event loop measures every frame, viewer or no viewer.
    app.set_viewer_view(28, 120);
    open_viewer(&mut app, &body);
    assert_eq!(
        app.viewer().map(|v| v.rows().len()),
        Some(28),
        "pushed and laid out, before a single key"
    );

    // The held keys, replayed with no layout of their own.
    press(&mut app, KeyCode::PageDown, NONE);
    let top = app.viewer().map(holoscommander::viewer::Viewer::top);
    assert!(
        top.is_some_and(|t| t > 200),
        "a page is a screenful, not a row: top is {top:?}"
    );
    let cursor_row = app.viewer().map(holoscommander::viewer::Viewer::cursor_row);
    assert_eq!(cursor_row, Some(0), "and the cursor kept its row index");

    // `Down` moves the cursor down the screen rather than scrolling it.
    press(&mut app, KeyCode::Down, NONE);
    assert_eq!(
        app.viewer().map(holoscommander::viewer::Viewer::top),
        top,
        "the window does not move until the cursor reaches its edge"
    );
    assert_eq!(
        app.viewer().map(holoscommander::viewer::Viewer::cursor_row),
        Some(1)
    );
}

#[test]
fn q_closes_outright_even_with_a_selection_live() {
    // the note spells the two-stage
    // rule for `Esc` alone, and `q` is an unambiguous request to leave. `F3`
    // used to be one too and is now find-next.
    let mut app = app_with(&["alpha"]);
    open_viewer(&mut app, "one\ntwo\n");
    app.service_viewer(2, 40);
    press(&mut app, KeyCode::Right, SHIFT);
    assert!(app.viewer().and_then(|v| v.selection()).is_some());
    press(&mut app, KeyCode::Char('q'), NONE);
    assert_eq!(app.viewer_depth(), 0, "`q` closes the viewer");
}

#[test]
fn tab_switches_the_hex_side_and_nothing_else_changes() {
    // "`Tab` moves the focus from one side to the other and
    // *nothing else changes*" (the design invariant 6).
    use holoscommander::config::ViewerMode;
    use holoscommander::viewer::select::HexSide;
    let mut app = app_with(&["alpha"]);
    open_viewer(&mut app, "abcdefghijklmnopqrstuvwxyz0123456789\n");
    press(&mut app, KeyCode::Char('2'), NONE);
    app.service_viewer(4, 80);
    assert_eq!(app.viewer().map(|v| v.mode()), Some(ViewerMode::Hex));
    for _ in 0..5 {
        press(&mut app, KeyCode::Right, SHIFT);
    }
    let before = app.viewer().expect("a viewer");
    let (sel, at, top, top_row, goal, row) = (
        before.selection(),
        before.cursor(),
        before.top(),
        before.top_row(),
        before.goal_column(),
        before.cursor_row(),
    );
    assert_eq!(sel.map(|s| s.len()), Some(5), "five bytes on the left");

    press(&mut app, KeyCode::Tab, NONE);
    let after = app.viewer().expect("a viewer");
    assert_eq!(after.hex_side(), HexSide::Chars, "the focus moved");
    assert_eq!(
        after.selection(),
        sel,
        "and the same five bytes are selected"
    );
    assert_eq!(after.cursor(), at);
    assert_eq!(after.top(), top);
    assert_eq!(after.top_row(), top_row);
    assert_eq!(after.goal_column(), goal);
    assert_eq!(after.cursor_row(), row);

    // "Pressing `Shift+Right` again extends the same selection to six."
    press(&mut app, KeyCode::Right, SHIFT);
    assert_eq!(
        app.viewer().and_then(|v| v.selection()).map(|s| s.len()),
        Some(6)
    );
}

#[test]
fn ctrl_a_selects_the_whole_file_and_ctrl_c_only_queues_a_copy() {
    // `Ctrl+A` "costs nothing" - and the design
    // invariant 13: `dispatch` still touches nothing, so `Ctrl+C` queues and
    // the event loop reads.
    let mut app = app_with(&["alpha"]);
    open_viewer(&mut app, "one\ntwo\nthree\n");
    app.service_viewer(3, 40);

    press(&mut app, KeyCode::Char('a'), CTRL);
    let sel = app
        .viewer()
        .and_then(|v| v.selection())
        .expect("the whole file");
    assert_eq!(sel.range().0, 0);
    assert_eq!(
        app.viewer().map(|v| v.top()),
        Some(0),
        "the window did not move"
    );
    assert_eq!(app.viewer().map(|v| v.cursor()), Some(0), "nor the cursor");

    press(&mut app, KeyCode::Char('c'), CTRL);
    assert!(
        app.take_viewer_copy().is_some(),
        "Ctrl+C left a request for the event loop rather than reading"
    );
    assert!(app.text_clipboard.is_none(), "and nothing was copied here");
}

#[test]
fn pure_scrolling_says_why_it_cannot_select() {
    // the design item 4 and invariant 17: with
    // `viewer.cursor = false` every selection key reports rather than doing
    // nothing - the "never a panic and never silence".
    let mut config = Config::default();
    config.viewer.cursor = false;
    let mut app = app_with_config(config, &["alpha"]);
    open_viewer(&mut app, "one\ntwo\nthree\nfour\nfive\n");
    app.service_viewer(2, 40);

    // The arrows are v0.4's page scroll to the byte.
    press(&mut app, KeyCode::Down, NONE);
    assert_eq!(
        app.viewer().map(|v| v.top()),
        Some(4),
        "the window scrolled"
    );

    for (code, mods) in [
        (KeyCode::Down, SHIFT),
        (KeyCode::Right, CTRL | SHIFT),
        (KeyCode::Char('a'), CTRL),
        (KeyCode::Char('b'), ALT),
    ] {
        app.message = None;
        press(&mut app, code, mods);
        assert_eq!(
            app.message.as_deref(),
            Some("the viewer has no cursor - set viewer.cursor = true to select"),
            "{code:?} {mods:?}"
        );
        assert_eq!(app.viewer().and_then(|v| v.selection()), None);
    }
}

#[test]
fn a_selection_is_a_byte_range_and_outlives_every_view_change() {
    // the design invariant 7 and the table: a byte range does not care how
    // the bytes are drawn, so nothing that only changes the view touches it.
    let mut app = app_with(&["alpha"]);
    open_viewer(&mut app, "alpha\nbeta\ngamma\ndelta\n");
    app.service_viewer(4, 40);
    for _ in 0..4 {
        press(&mut app, KeyCode::Right, SHIFT);
    }
    let sel = app
        .viewer()
        .and_then(|v| v.selection())
        .expect("a selection");

    for (code, mods, what) in [
        (KeyCode::Char('2'), NONE, "hex mode"),
        (KeyCode::Char('g'), NONE, "the hex grouping"),
        (KeyCode::Char('d'), NONE, "the hex format"),
        (KeyCode::Char('e'), NONE, "the byte order"),
        (KeyCode::Char('1'), NONE, "text mode"),
        (KeyCode::Char('w'), NONE, "wrap"),
        (KeyCode::F(8), NONE, "the encoding"),
        (KeyCode::F(2), NONE, "a reload"),
    ] {
        press(&mut app, code, mods);
        app.service_viewer(4, 40);
        assert_eq!(
            app.viewer().and_then(|v| v.selection()),
            Some(sel),
            "{what} kept the selection"
        );
    }
}

#[test]
fn the_find_bar_takes_typing_rather_than_the_keys_those_letters_are_bound_to() {
    // the bar "behaves like the panel quick search: typing
    // searches immediately". Which means that while it is open, `n` is a
    // letter - the alternative is a bar you cannot type `n` into.
    let mut app = app_with(&["alpha"]);
    open_viewer(&mut app, "one\nnine\nten\n");
    app.service_viewer(3, 40);

    press(&mut app, KeyCode::F(7), NONE);
    assert!(
        app.viewer().is_some_and(|v| v.find().is_open()),
        "F7 opens the bar"
    );
    for c in "nine".chars() {
        press(&mut app, KeyCode::Char(c), NONE);
    }
    assert_eq!(
        app.viewer().map(|v| v.find().input().to_string()),
        Some("nine".to_string()),
        "every letter went into the bar, `n` included"
    );
    assert_eq!(
        app.viewer().and_then(|v| v.find().current()),
        Some(4),
        "and the search ran on each keystroke"
    );
    assert_eq!(app.viewer_depth(), 1, "`n` did not step and nothing closed");

    // `Esc` closes the bar and keeps the match, so `n` steps from here.
    press(&mut app, KeyCode::Esc, NONE);
    assert!(
        app.viewer().is_some_and(|v| !v.find().is_open()),
        "Esc closes the bar"
    );
    assert_eq!(
        app.viewer_depth(),
        1,
        "and closes the bar rather than the viewer"
    );
    assert_eq!(
        app.viewer().and_then(|v| v.find().current()),
        Some(4),
        "keeping the position"
    );

    // Opening the bar again gives an **empty** one: `F7` and `Ctrl+F` mean
    // "search for something", and the something is what is about to be typed.
    // A bar that arrived holding the last pattern meant deleting it a
    // character at a time before a different search could start. The
    // remembered pattern is not lost - it is what `n` steps.
    press(&mut app, KeyCode::F(7), NONE);
    assert_eq!(
        app.viewer().map(|v| v.find().input().to_string()),
        Some(String::new()),
        "the bar opens empty"
    );

    // Backspace inside the bar edits it rather than navigating.
    for ch in "nine".chars() {
        press(&mut app, KeyCode::Char(ch), NONE);
    }
    press(&mut app, KeyCode::Backspace, NONE);
    assert_eq!(
        app.viewer().map(|v| v.find().input().to_string()),
        Some("nin".to_string())
    );
}

#[test]
fn n_and_shift_n_step_through_matches_without_the_bar() {
    // "`n`/`Shift+N` next/previous".
    let mut app = app_with(&["alpha"]);
    open_viewer(&mut app, "aXbXcXd\n");
    app.service_viewer(3, 40);

    press(&mut app, KeyCode::F(7), NONE);
    press(&mut app, KeyCode::Char('X'), NONE);
    press(&mut app, KeyCode::Enter, NONE);
    assert_eq!(app.viewer().and_then(|v| v.find().current()), Some(1));

    press(&mut app, KeyCode::Char('n'), NONE);
    assert_eq!(
        app.viewer().and_then(|v| v.find().current()),
        Some(3),
        "`n` is the action again once the bar is closed"
    );
    press(&mut app, KeyCode::Char('n'), NONE);
    assert_eq!(app.viewer().and_then(|v| v.find().current()), Some(5));
    press(&mut app, KeyCode::Char('N'), SHIFT);
    assert_eq!(
        app.viewer().and_then(|v| v.find().current()),
        Some(3),
        "`Shift+N` goes back"
    );

    // the design does not say `n` wraps, and on a file whose count is still
    // filling in there is no way to know this is the last match without
    // finishing the scan. So the end says so rather than guessing.
    press(&mut app, KeyCode::Char('n'), NONE);
    press(&mut app, KeyCode::Char('n'), NONE);
    press(&mut app, KeyCode::Char('n'), NONE);
    assert!(
        app.message
            .as_deref()
            .is_some_and(|m| m.contains("not found")),
        "{:?}",
        app.message
    );
}

#[test]
fn a_manual_switch_holds_for_this_file_and_is_not_carried_to_the_next() {
    // The mode is a reading of the file, and the next file is a different
    // file. Switching to hex here says something about *this* file; carrying
    // it onto the next one is answering a question nobody asked twice. The
    // choice therefore lives on the viewer and not on the application, which
    // is why there is no longer a remembered mode to inspect.
    use holoscommander::config::ViewerMode;
    let mut app = app_with(&["alpha"]);

    open_viewer(&mut app, "one\ntwo\n");
    app.service_viewer(3, 40);
    assert_eq!(app.viewer().map(|v| v.mode()), Some(ViewerMode::Text));

    press(&mut app, KeyCode::Char('2'), NONE);
    assert_eq!(app.viewer().map(|v| v.mode()), Some(ViewerMode::Hex));
    // It holds for as long as this file is open: laying out again, and any
    // other key, leaves it in hex.
    app.service_viewer(3, 40);
    press(&mut app, KeyCode::Down, NONE);
    assert_eq!(
        app.viewer().map(|v| v.mode()),
        Some(ViewerMode::Hex),
        "the switch did not hold within the file"
    );

    // The next file opens by its own content, not by that choice.
    press(&mut app, KeyCode::Esc, NONE);
    open_viewer(&mut app, "plain text, nothing binary about it\n");
    app.service_viewer(3, 40);
    assert_eq!(
        app.viewer().map(|v| v.mode()),
        Some(ViewerMode::Text),
        "the last file's hex followed us to a text file"
    );
}

#[test]
fn esc_and_q_close_the_viewer_and_give_the_panel_back() {
    for key in [
        KeyEvent::new(KeyCode::Esc, NONE),
        KeyEvent::new(KeyCode::Char('q'), NONE),
    ] {
        let mut app = app_with(&["alpha"]);
        open_viewer(&mut app, "hello\n");
        assert_eq!(app.focus, Focus::Viewer);
        dispatch(&mut app, key).expect("dispatch");
        assert_eq!(app.focus, Focus::Panel(Side::Left), "{key:?}");
        assert_eq!(app.viewer_depth(), 0, "{key:?}");
    }
}

#[test]
fn f3_in_the_viewer_finds_rather_than_closing() {
    // `F3` used to be a second way out; what you most often
    // want after opening a file out of a search is the next hit inside it.
    let mut app = app_with(&["alpha"]);
    open_viewer(&mut app, "one\ntwo\none\n");
    app.service_viewer(3, 40);
    dispatch(&mut app, KeyEvent::new(KeyCode::F(3), NONE)).expect("dispatch");
    assert_eq!(app.viewer_depth(), 1, "F3 did not close the viewer");
}

#[test]
fn a_find_becomes_the_sessions_pattern_and_the_next_viewer_opens_with_it() {
    // the pattern outlives the viewer that ran it, so `F3`
    // into the next file walks that file's matches without retyping.
    use holoscommander::viewer::find::{FindKind, FindQuery};
    let mut app = app_with(&["alpha"]);
    open_viewer(&mut app, "needle here\nand needle again\n");
    app.service_viewer(3, 40);

    // Type a pattern into the bar and step to a match: that is what "searched"
    // means, and only then is it the session's.
    app.viewer_mut().expect("viewer").seed_find(FindQuery {
        input: "needle".to_string(),
        kind: FindKind::Text,
        case: holoscommander::config::QuickSearchCase::Insensitive,
    });
    dispatch(&mut app, KeyEvent::new(KeyCode::F(3), NONE)).expect("dispatch");
    assert_eq!(
        app.viewers.last_find.as_ref().map(|q| q.input.as_str()),
        Some("needle"),
        "the find became the session's pattern"
    );

    // And it survives the viewer it was typed into.
    dispatch(&mut app, KeyEvent::new(KeyCode::Char('q'), NONE)).expect("dispatch");
    assert_eq!(app.viewer_depth(), 0);
    assert_eq!(
        app.viewers.last_find.as_ref().map(|q| q.input.as_str()),
        Some("needle")
    );
}

#[test]
fn f1_in_the_viewer_opens_the_viewers_own_help_page() {
    // "`F1` in the viewer opens the viewer keys", and "the help
    // view uses the same viewer machinery".
    use holoscommander::app::ViewRequest;
    use holoscommander::viewer::Viewer;

    let mut app = app_with(&["alpha"]);
    open_viewer(&mut app, "hello\n");
    press(&mut app, KeyCode::F(1), NONE);
    let (title, body) = match app.take_pending_view() {
        Some(ViewRequest::Text { title, body, help }) => {
            assert!(help, "it is a help page and says so");
            (title, body)
        }
        other => panic!("expected a generated page, got {other:?}"),
    };
    assert_eq!(title, "Viewer keys");
    assert!(body.contains("Close the viewer"), "{body}");

    // Servicing it stacks a second viewer over the first, and Esc unwinds one
    // level at a time.
    let id = app.next_viewer_id();
    let cfg = app.config.viewer.clone();
    let mut page = Viewer::open_memory(id, title, body, &cfg).expect("open help");
    page.mark_help();
    app.push_viewer(page);
    assert_eq!(app.viewer_depth(), 2);
    assert!(
        app.viewer()
            .is_some_and(holoscommander::viewer::Viewer::is_help)
    );

    // `F1` on the help page does not stack another.
    press(&mut app, KeyCode::F(1), NONE);
    assert!(app.pending_view().is_none());

    press(&mut app, KeyCode::Esc, NONE);
    assert_eq!(app.viewer_depth(), 1, "back to the file");
    assert_eq!(app.focus, Focus::Viewer);
    press(&mut app, KeyCode::Esc, NONE);
    assert_eq!(app.focus, Focus::Panel(Side::Left));
}

#[test]
fn ctrl_g_in_the_viewer_asks_for_an_offset_and_accepts_0x() {
    // "`Ctrl+G` jumps to an offset, accepting `0x` notation."
    use holoscommander::config::ViewerMode;
    use holoscommander::dialog::DialogResult;
    use holoscommander::input::{DialogId, dialog_accepted};

    let mut app = app_with(&["alpha"]);
    open_viewer(&mut app, &"0123456789abcdef".repeat(64));
    if let Some(v) = app.viewer_mut() {
        v.set_mode(ViewerMode::Hex).expect("hex");
    }
    press(&mut app, KeyCode::Char('g'), CTRL);
    assert_eq!(app.focus, Focus::Dialog(DialogId::GotoOffset));

    dialog_accepted(
        &mut app,
        DialogId::GotoOffset,
        DialogResult::Text("0x40".to_string()),
    );
    assert_eq!(
        app.viewer().map(holoscommander::viewer::Viewer::cursor),
        Some(0x40)
    );

    // A value that will not parse is reported and moves nothing.
    dialog_accepted(
        &mut app,
        DialogId::GotoOffset,
        DialogResult::Text("nonsense".to_string()),
    );
    assert_eq!(
        app.viewer().map(holoscommander::viewer::Viewer::cursor),
        Some(0x40)
    );
    assert!(
        app.message
            .as_deref()
            .unwrap_or_default()
            .contains("not a position"),
        "{:?}",
        app.message
    );

    // The other two forms the design names. A percentage and a line number
    // are answered rather than refused, and neither is read as an offset.
    dialog_accepted(
        &mut app,
        DialogId::GotoOffset,
        DialogResult::Text("50%".to_string()),
    );
    assert_eq!(
        app.viewer().map(holoscommander::viewer::Viewer::cursor),
        Some(512),
        "50% of a 1024-byte file"
    );
    dialog_accepted(
        &mut app,
        DialogId::GotoOffset,
        DialogResult::Text(":3".to_string()),
    );
    assert_eq!(
        app.viewer().map(holoscommander::viewer::Viewer::top),
        Some(0),
        "a file with no line breaks has one line, and :3 lands on what there is"
    );
}

#[test]
fn a_dialog_over_the_viewer_returns_focus_to_the_viewer() {
    // a dialog consumes all input, and closing it puts focus back
    // where it came from - which here is the viewer, not the panel.
    use holoscommander::input::DialogId;
    let mut app = app_with(&["alpha"]);
    open_viewer(&mut app, "hello\n");
    press(&mut app, KeyCode::Char('g'), CTRL);
    assert_eq!(app.focus, Focus::Dialog(DialogId::GotoOffset));
    press(&mut app, KeyCode::Esc, NONE);
    assert_eq!(app.focus, Focus::Viewer);
    assert_eq!(app.viewer_depth(), 1);
}

#[test]
fn a_viewer_opened_from_the_command_line_gives_the_command_line_back() {
    // `F3` is a `[global]` binding and resolves from the
    // command line, where a half-written command is sitting with its caret
    // where the user left it. Closing the viewer used to hand focus to the
    // panel unconditionally - so the next character typed started a quick
    // search instead of finishing the command. The dialog stack has recorded
    // the focus it displaced since v0.1; the viewer stack did not.
    let mut app = app_with(&["alpha", "beta"]);
    app.set_focus(Focus::CommandLine);
    app.cmdline.set_text("grep foo ");
    open_viewer(&mut app, "hello\nworld\n");
    assert_eq!(app.focus, Focus::Viewer);

    press(&mut app, KeyCode::Esc, NONE);
    assert_eq!(
        app.focus,
        Focus::CommandLine,
        "back to where `F3` was pressed"
    );
    assert_eq!(app.cmdline.text(), "grep foo ", "with the command intact");

    // From a panel it is still the panel, and the help page over a file still
    // returns to the file.
    app.set_focus(Focus::Panel(Side::Left));
    open_viewer(&mut app, "hello\n");
    open_viewer(&mut app, "the help page\n");
    assert_eq!(app.viewer_depth(), 2);
    press(&mut app, KeyCode::Esc, NONE);
    assert_eq!(app.focus, Focus::Viewer, "the file underneath");
    press(&mut app, KeyCode::Esc, NONE);
    assert_eq!(app.focus, Focus::Panel(Side::Left));
}

#[test]
fn a_key_the_viewer_swallows_says_so_rather_than_naming_a_milestone() {
    // the "never a panic and never silence": a key that does nothing
    // says *why*. `F10`, `F5`, `Ctrl+O` and `Tab` are all shipped, bound and
    // working one keystroke away - they are swallowed because the design says
    // the viewer consumes all input, which is not the same fact as "this
    // milestone has not brought it yet".
    let mut app = app_with(&["alpha"]);
    open_viewer(&mut app, "hello\n");
    for (code, mods) in [
        (KeyCode::F(10), NONE),
        (KeyCode::F(5), NONE),
        (KeyCode::Char('o'), CTRL),
        // `Tab` is no longer one of these: the design gives it to the hex
        // side switch and `[viewer] hex_side = ["tab"]` binds it, so it is a
        // viewer key that acts rather than one the viewer swallows.
    ] {
        app.message = None;
        press(&mut app, code, mods);
        let said = app.message.clone().unwrap_or_default();
        // Swallowed **and silent**. These are the panel's keys, and the panels
        // are not what the reader is looking at; answering "not available in
        // the viewer" on every stray keystroke turned the status line into a
        // list of things that had not happened.
        assert!(
            said.is_empty(),
            "{code:?} was answered rather than ignored: {said:?}"
        );
        assert!(
            !said.contains("nothing bound to do yet"),
            "{code:?} is bound and works: {said:?}"
        );
        assert_eq!(app.focus, Focus::Viewer, "and it stays in the viewer");
    }
}

// --------------------------------------------- the design accelerators --

/// through the dispatcher rather than through `handle_key`:
/// inside a dialog, the dialog's mnemonics beat the global `Alt`
/// bindings (the design I4).
///
/// > `Alt+S` is **Search for** while one is open and `search` when one is not.
///
/// The claim is about the dispatcher, so it is tested there: `Alt+S` on a
/// panel opens the Find dialog, and `Alt+S` again reaches the dialog's own
/// `Search for` field instead of opening a second one. `Keymap::resolve` still
/// answers `Some(Action::Search)` for the key - step 2 falls back
/// to `[global]` - so this is the ordering inside `handle_key` doing its job
/// and not an accident of the key being unbound.
#[test]
fn a_dialog_mnemonic_beats_the_global_alt_binding_of_the_same_letter() {
    let mut app = app_with(&["one.txt", "two.txt"]);
    press(&mut app, KeyCode::Char('s'), ALT);
    assert_eq!(app.dialogs().len(), 1, "Alt+S opened the Find dialog");

    // A checkbox has no caret, so the cursor is the trait-level way to see
    // where the focus is without reaching inside the dialog.
    let area = ratatui_area();
    press(&mut app, KeyCode::Char('v'), ALT);
    assert_eq!(app.dialogs().len(), 1, "Alt+V is a checkbox, not an action");
    assert!(
        app.top_dialog().and_then(|d| d.cursor(area)).is_none(),
        "Alt+V put the focus on `Search archives`, which has no caret"
    );

    // `Alt+S` is the dialog's Start search button and `search` globally, and
    // the dialog wins: it accepts and closes. Had the global binding run
    // instead, a SECOND Find dialog would have been pushed on top of this one
    // and the count would be two.
    press(&mut app, KeyCode::Char('s'), ALT);
    assert_eq!(
        app.dialogs().len(),
        0,
        "a dialog consumes all input, so `search` never ran and \
         no second Find dialog was pushed"
    );
    // And `Alt+F` is the field it used to be: the mnemonic still
    // moves the caret, it is just on the letter Total Commander underlines.
    press(&mut app, KeyCode::Char('s'), ALT);
    press(&mut app, KeyCode::Char('v'), ALT);
    press(&mut app, KeyCode::Char('f'), ALT);
    assert!(
        app.top_dialog().and_then(|d| d.cursor(area)).is_some(),
        "Alt+F moved the caret into `Search for`, which the design names"
    );
}

/// "`Alt` with a *digit* is already the tab strip, so mnemonics
/// are letters and the two never collide" (the design I5), through
/// the dispatcher.
///
/// `Alt+2` and `Alt+3` change tab; `Alt+1` comes back. None of them is a
/// mnemonic and none of them leaves the dialog, and the letters keep working
/// on whichever tab is open - `Alt+M` is `Modified` on Advanced and names
/// nothing on General.
#[test]
fn alt_digit_still_walks_the_tab_strip_while_alt_letter_is_a_mnemonic() {
    let mut app = app_with(&["one.txt"]);
    press(&mut app, KeyCode::Char('s'), ALT);
    let general = letters(&app);
    assert!(general.contains(&'t'), "General has `Find text`");
    assert!(!general.contains(&'z'), "`Size <=` is on Advanced");

    press(&mut app, KeyCode::Char('2'), ALT);
    assert_eq!(
        app.dialogs().len(),
        1,
        "Alt+2 is the tab strip, not an action"
    );
    let advanced = letters(&app);
    assert!(advanced.contains(&'z'), "Alt+2 opened Advanced");
    assert!(!advanced.contains(&'t'), "`Find text` is on General");

    press(&mut app, KeyCode::Char('1'), ALT);
    assert_eq!(letters(&app), general, "Alt+1 came back to General");
    for digit in '0'..='9' {
        press(&mut app, KeyCode::Char(digit), ALT);
        assert_eq!(
            app.dialogs().len(),
            1,
            "Alt+{digit} is never a mnemonic and never leaves the dialog"
        );
    }
}

/// The open dialog's mnemonics, sorted, so two screens can be compared.
fn letters(app: &App) -> Vec<char> {
    let mut out = app
        .top_dialog()
        .map(holoscommander::dialog::Dialog::mnemonic_letters)
        .unwrap_or_default();
    out.sort_unstable();
    out
}

/// A dialog-sized rectangle, for the trait's `cursor`.
fn ratatui_area() -> Rect {
    Rect::new(0, 0, 100, 30)
}

#[test]
fn delete_in_the_console_edits_the_line_and_does_not_offer_to_delete_a_folder() {
    // the command line IS the shell's own input line, so the
    // keys that edit a line of text are the shell's whatever a global binding
    // says. `Delete` is bound globally to the file operation `F8` runs, so
    // pressing it to rub out a character raised "Delete the folder?" over the
    // panel - one stray Enter from deleting a directory nobody was looking at.
    let mut app = app_with(&["alpha", "beta"]);
    app.focus = Focus::Console;
    for code in [
        KeyCode::Delete,
        KeyCode::Backspace,
        KeyCode::Left,
        KeyCode::Right,
        KeyCode::Home,
        KeyCode::End,
        KeyCode::Insert,
    ] {
        let _ = app.take_pending_shell();
        dispatch(&mut app, KeyEvent::new(code, NONE)).expect("dispatch");
        assert!(
            !app.dialog_is_open(),
            "{code:?} opened a dialog instead of reaching the shell"
        );
        assert!(
            app.message.is_none(),
            "{code:?} said something instead of being the shell's: {:?}",
            app.message
        );
    }

    // Shift+Delete is the permanent delete and is NOT a
    // line-editing key, so it still reaches the application. With nothing
    // marked and a cursor on a real row it raises the confirmation, which is
    // exactly what the bare key must no longer do.
    dispatch(&mut app, KeyEvent::new(KeyCode::Delete, SHIFT)).expect("dispatch");
    assert!(
        app.dialog_is_open() || app.message.is_some(),
        "Shift+Delete is still the application's"
    );
}

#[test]
fn goto_on_a_remote_panel_stays_on_the_remote() {
    // Reported from a real session: Ctrl+G on a connected panel took the
    // typed path to the LOCAL filesystem. The answer was handed to
    // `VfsPath::local` unconditionally, so the panel walked out of the
    // connection without saying so.
    use holoscommander::dialog::DialogResult;
    use holoscommander::input::DialogId;
    use holoscommander::remote::RemoteId;
    use holoscommander::vfs::BackendKind;

    let mut app = app_with(&["alpha"]);
    let id = RemoteId(3);
    app.left.active_tab_mut().path = id.path("/srv/media");

    holoscommander::input::dialog_answered(
        &mut app,
        DialogId::GotoPath,
        None,
        DialogResult::Text("/var/log".to_string()),
    );

    let now = &app.left.active_tab().path;
    assert!(
        matches!(now.backend(), BackendKind::Remote(got) if got == id),
        "the panel stayed on the connection: {now}"
    );
    assert_eq!(now.tail(), std::path::Path::new("/var/log"));
}

#[test]
fn goto_on_a_local_panel_is_unchanged() {
    use holoscommander::dialog::DialogResult;
    use holoscommander::input::DialogId;
    use holoscommander::vfs::BackendKind;
    let mut app = app_with(&["alpha"]);
    holoscommander::input::dialog_answered(
        &mut app,
        DialogId::GotoPath,
        None,
        DialogResult::Text("/tmp".to_string()),
    );
    let now = &app.left.active_tab().path;
    assert!(matches!(now.backend(), BackendKind::Local), "{now}");
}
