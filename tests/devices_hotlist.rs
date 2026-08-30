//! the device pickers and the hotlist, driven the way the
//! event loop drives them: synthetic [`KeyEvent`]s through [`dispatch`], then
//! the dialog's answer through [`dialog_accepted`], with assertions on [`App`]
//! state and no terminal anywhere.
//!
//! The popup's own behaviour - the rows, the separator, the nine-row window,
//! the quick search, the greyed row - is unit-tested in
//! `crate::ui::dialog::drives`, and the file format in
//! `crate::devices::hotlist`. What is here is the half that needs an
//! application: which panel a key acts on, and what an answered prompt changes.
//!
//! Invariants I1 and I6 of the design are asserted here, by name.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "an integration test is its own crate, so it is not #[cfg(test)] \
              and clippy.toml's allow-*-in-tests keys do not reach it. \
              Panicking assertions are the point of a test."
)]

use holoscommander::app::{App, DrivesRequest};
use holoscommander::config::{Config, Keymap, Theme};
use holoscommander::dialog::DialogResult;
use holoscommander::input::{
    DialogId, Focus, KeyCode, KeyEvent, KeyModifiers, dialog_accepted, dispatch,
};
use holoscommander::panel::Side;
use holoscommander::vfs::VfsPath;

const NONE: KeyModifiers = KeyModifiers::NONE;
const ALT: KeyModifiers = KeyModifiers::ALT;

/// A headless app with both panels somewhere real and known.
fn app() -> App {
    let mut app = App::headless(Config::default(), Keymap::builtin(), Theme::blue());
    app.navigate(Side::Left, VfsPath::local("/home/thorin/left"));
    app.navigate(Side::Right, VfsPath::local("/home/thorin/right"));
    let _ = app.take_pending_reads();
    app
}

fn press(app: &mut App, code: KeyCode, mods: KeyModifiers) {
    dispatch(app, KeyEvent::new(code, mods)).expect("dispatch never fails on a synthetic key");
}

fn path_of(app: &App, side: Side) -> String {
    app.panel(side).active_tab().path.to_string()
}

/// **I1.** "`Alt+F1` always targets the left panel and `Alt+F2`
/// always the right, independent of which panel has focus, and focus moves to
/// the targeted panel." It is a spatial command, not a "do it to the current
/// one" command.
#[test]
fn alt_f1_with_the_right_panel_active_changes_the_left_one() {
    let mut app = app();
    app.set_focus(Focus::Panel(Side::Right));
    assert_eq!(app.active_side, Side::Right);

    press(&mut app, KeyCode::F(1), ALT);
    // Queued rather than built: enumerating mounts reads /proc/mounts and
    // `dispatch` may not read.
    assert_eq!(
        app.drives_pending(),
        Some(DrivesRequest::Devices(Side::Left)),
        "alt+F1 asks for the LEFT panel's popup while the right one has focus"
    );

    // The event loop builds it, and the popup names the side it will change.
    app.service_drives();
    assert_eq!(app.focus, Focus::Dialog(DialogId::Drive(Side::Left)));

    // Answering it navigates that panel and moves focus there.
    dialog_accepted(
        &mut app,
        DialogId::Drive(Side::Left),
        DialogResult::Text("/tmp".to_string()),
    );
    assert_eq!(path_of(&app, Side::Left), "/tmp");
    assert_eq!(
        path_of(&app, Side::Right),
        "/home/thorin/right",
        "the panel that had focus is untouched"
    );
    assert_eq!(
        app.active_side,
        Side::Left,
        "focus follows the panel the key named"
    );
}

/// The other half of the same rule: `Alt+F2` is the right panel, from the left.
#[test]
fn alt_f2_with_the_left_panel_active_changes_the_right_one() {
    let mut app = app();
    assert_eq!(app.active_side, Side::Left);

    press(&mut app, KeyCode::F(2), ALT);
    assert_eq!(
        app.drives_pending(),
        Some(DrivesRequest::Devices(Side::Right))
    );
    app.service_drives();
    dialog_accepted(
        &mut app,
        DialogId::Drive(Side::Right),
        DialogResult::Text("/tmp".to_string()),
    );
    assert_eq!(path_of(&app, Side::Right), "/tmp");
    assert_eq!(path_of(&app, Side::Left), "/home/thorin/left");
    assert_eq!(app.active_side, Side::Right);
}

/// `Ctrl+D` "opens that same list **alone**", acting on the
/// **active** panel rather than on a fixed side.
#[test]
fn ctrl_d_is_the_hotlist_alone_and_acts_on_whichever_panel_has_focus() {
    let mut app = app();
    app.set_focus(Focus::Panel(Side::Right));

    press(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
    assert_eq!(app.drives_pending(), Some(DrivesRequest::Hotlist));

    app.service_drives();
    assert_eq!(app.focus, Focus::Dialog(DialogId::Hotlist));

    dialog_accepted(
        &mut app,
        DialogId::Hotlist,
        DialogResult::Text("/tmp".to_string()),
    );
    assert_eq!(path_of(&app, Side::Right), "/tmp");
    assert_eq!(path_of(&app, Side::Left), "/home/thorin/left");
}

/// **I6.** "`Ctrl+Shift+D` adds the active panel's directory,
/// label pre-filled from the last path component and editable, and a duplicate
/// path replaces the existing entry's label rather than adding a second row."
///
/// The order is never touched either: `hotlist.toml` holds the entries in the
/// order the user put them in, so the replacement happens **in place**.
#[test]
fn ctrl_shift_d_twice_on_one_directory_leaves_one_row_where_the_first_was() {
    let mut app = app();
    // A first, unrelated entry, so "in place" has somewhere to be wrong.
    app.add_to_hotlist("elsewhere".to_string(), "/var/log".to_string());

    press(
        &mut app,
        KeyCode::Char('D'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    );
    assert_eq!(app.focus, Focus::Dialog(DialogId::HotlistAdd));
    dialog_accepted(
        &mut app,
        DialogId::HotlistAdd,
        DialogResult::Text("first".to_string()),
    );

    press(
        &mut app,
        KeyCode::Char('D'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    );
    dialog_accepted(
        &mut app,
        DialogId::HotlistAdd,
        DialogResult::Text("second".to_string()),
    );

    let labels: Vec<&str> = app
        .hotlist
        .entries()
        .iter()
        .map(|e| e.label.as_str())
        .collect();
    let paths: Vec<&str> = app
        .hotlist
        .entries()
        .iter()
        .map(|e| e.path.as_str())
        .collect();
    assert_eq!(
        paths,
        vec!["/var/log", "/home/thorin/left"],
        "one row for the directory, in the position the first add gave it"
    );
    assert_eq!(
        labels,
        vec!["elsewhere", "second"],
        "the second label replaced the first"
    );
    assert!(
        app.hotlist.is_dirty(),
        "and the file needs writing, which the event loop does"
    );
}

/// The label the prompt opens with is the last path component,
/// and it is editable rather than fixed - so answering with
/// something else is what is stored.
#[test]
fn the_label_prompt_starts_at_the_last_path_component() {
    let mut app = app();
    press(
        &mut app,
        KeyCode::Char('D'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    );
    // The prompt is an InputDialog seeded from the panel path; `Enter` with no
    // editing stores exactly what it was seeded with.
    press(&mut app, KeyCode::Enter, NONE);
    assert_eq!(app.hotlist.entries().len(), 1);
    assert_eq!(
        app.hotlist.entries().first().map(|e| e.label.as_str()),
        Some("left"),
        "seeded from the last component of /home/thorin/left"
    );
}

/// the hotlist holds **local** directories. A panel showing an
/// archive or a remote host has no local path to store, and the refusal says
/// so rather than storing a path that cannot be reopened.
#[test]
fn a_panel_that_is_not_local_is_refused_rather_than_stored() {
    let mut app = app();
    app.left.active_tab_mut().path =
        VfsPath::new(holoscommander::vfs::BackendKind::List, "/results");
    press(
        &mut app,
        KeyCode::Char('D'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    );
    assert!(!app.dialog_is_open(), "asking for a label first is the bug");
    assert!(app.hotlist.entries().is_empty());
    let message = app.message.as_deref().unwrap_or_default();
    assert!(message.contains("local directories"), "{message}");
}
