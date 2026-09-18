//! The send from the application's side: the queue, the picker, the job.

use super::*;
use crate::config::{Config, Keymap, Theme};
use crate::input::{DialogId, Focus, KeyCode, KeyEvent, KeyModifiers, dispatch};
use crate::localsend::fake::{Mode, Receiver, scratch};

fn app() -> App {
    App::headless(Config::default(), Keymap::builtin(), Theme::blue())
}

#[test]
fn nothing_selected_is_a_message_and_not_a_picker() {
    let mut app = app();
    app.send_paths(Vec::new());
    assert!(!app.send_paths_queued());
    assert!(
        app.message
            .as_deref()
            .is_some_and(|m| m.contains("nothing to send")),
        "{:?}",
        app.message
    );
}

#[test]
fn a_send_puts_the_picker_up_with_discovery_behind_it_and_esc_takes_both_down() {
    let (dir, paths) = scratch("app-picker");
    let mut app = app();
    app.send_paths(paths);
    assert!(app.send_paths_queued(), "queued for the loop");
    assert!(!app.is_discovering_devices(), "nothing runs in dispatch");
    app.service_localsend();
    assert!(app.is_choosing_device());
    assert_eq!(
        app.top_dialog().map(|d| d.id()),
        Some(DialogId::SendDevice),
        "the picker is up, whether or not this machine can join the group"
    );
    // A second request while it is up is refused, not stacked.
    app.send_paths(vec![dir.clone()]);
    assert!(
        app.message
            .as_deref()
            .is_some_and(|m| m.contains("already choosing"))
    );
    app.stop_device_discovery();
    assert!(!app.is_discovering_devices());
    assert!(!app.is_choosing_device());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn choosing_a_device_queues_a_send_job_that_reaches_the_device() {
    let (dir, paths) = scratch("app-send");
    let receiver = Receiver::start(Mode::Accept);
    let mut app = app();
    app.send_paths(paths);
    app.service_localsend();
    let choice = App::encode_peer(&receiver.peer(), "");
    app.answer_send_device(&choice);
    assert!(
        !app.is_discovering_devices(),
        "discovery ends with the choice"
    );
    assert!(!app.is_choosing_device());
    assert!(
        app.message
            .as_deref()
            .is_some_and(|m| m.contains("sending 2 items to Fake Phone")),
        "{:?}",
        app.message
    );
    let rows = app.jobs.rows();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].kind, JobKind::LocalSend);
    let spec = app.jobs.spec(rows[0].id).expect("the spec");
    assert_eq!(spec.sources.len(), 2);
    let request = spec.options.localsend.as_ref().expect("a request");
    assert_eq!(request.peer.alias, "Fake Phone");
    assert_eq!(request.pin, None);
    assert!(request.me.alias.starts_with("hcmd"));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_pin_typed_in_the_picker_rides_with_the_job() {
    let (dir, paths) = scratch("app-pin");
    let receiver = Receiver::start(Mode::Pin);
    let mut app = app();
    app.send_paths(paths);
    app.service_localsend();
    app.answer_send_device(&App::encode_peer(&receiver.peer(), "1234"));
    let rows = app.jobs.rows();
    let spec = app.jobs.spec(rows[0].id).expect("the spec");
    assert_eq!(
        spec.options
            .localsend
            .as_ref()
            .and_then(|r| r.pin.as_deref()),
        Some("1234")
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn an_unreadable_choice_sends_nothing_and_stops_discovery() {
    let (dir, paths) = scratch("app-junk");
    let mut app = app();
    app.send_paths(paths);
    app.service_localsend();
    app.answer_send_device("not a choice");
    assert!(!app.is_discovering_devices());
    assert!(app.jobs.rows().is_empty());
    assert!(
        app.message
            .as_deref()
            .is_some_and(|m| m.contains("no device"))
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn the_alias_names_this_machine() {
    let alias = alias();
    assert!(alias.starts_with("hcmd"), "{alias}");
}

/// The bug the first real use found: cancel the picker, and every later send
/// said "already choosing a device". A bare `DialogOutcome::Cancel` popped the
/// dialog without the application hearing, so the queued paths stayed. This
/// goes through the real dispatcher with a real Esc.
#[test]
fn esc_in_the_picker_reaches_the_application_and_a_later_send_is_not_refused() {
    let (dir, paths) = scratch("app-esc");
    let mut app = app();
    app.send_paths(paths.clone());
    app.service_localsend();
    assert!(app.is_choosing_device());
    assert_eq!(app.focus, Focus::Dialog(DialogId::SendDevice));

    dispatch(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)).expect("dispatch");
    assert!(app.top_dialog().is_none(), "the picker is gone");
    assert!(!app.is_choosing_device(), "and so are the queued paths");
    assert!(!app.is_discovering_devices(), "and discovery");

    app.message = None;
    app.send_paths(paths);
    assert!(app.send_paths_queued(), "a second send is taken");
    assert!(app.message.is_none(), "no refusal: {:?}", app.message);
    app.stop_device_discovery();
    let _ = std::fs::remove_dir_all(dir);
}

/// The Cancel button, through the dispatcher: Tab to it, Enter.
#[test]
fn the_cancel_button_puts_discovery_and_the_paths_away_too() {
    let (dir, paths) = scratch("app-cancel-button");
    let mut app = app();
    app.send_paths(paths);
    app.service_localsend();
    let press = |app: &mut App, code: KeyCode| {
        dispatch(app, KeyEvent::new(code, KeyModifiers::NONE)).expect("dispatch");
    };
    for _ in 0..4 {
        press(&mut app, KeyCode::Tab);
    }
    press(&mut app, KeyCode::Enter);
    assert!(app.top_dialog().is_none(), "the picker is gone");
    assert!(!app.is_choosing_device());
    assert!(!app.is_discovering_devices());
    assert!(app.jobs.rows().is_empty(), "and nothing was sent");
    let _ = std::fs::remove_dir_all(dir);
}

/// A send queued but not yet serviced is put away by a stop as well, so a
/// stop can never leave a picker to pop up later over nothing.
#[test]
fn a_stop_before_the_loop_serviced_the_send_drops_it() {
    let (dir, paths) = scratch("app-pending");
    let mut app = app();
    app.send_paths(paths.clone());
    assert!(app.send_paths_queued());
    app.stop_device_discovery();
    assert!(!app.send_paths_queued());
    app.service_localsend();
    assert!(app.top_dialog().is_none(), "nothing to put up");
    app.send_paths(paths);
    assert!(app.send_paths_queued(), "and a new send is taken");
    app.stop_device_discovery();
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_folder_is_counted_on_a_thread_and_the_picker_learns_its_files_and_bytes() {
    let (dir, paths) = scratch("app-count");
    let mut app = app();
    app.send_paths(paths);
    app.service_localsend();
    let mut summary = None;
    for _ in 0..100 {
        app.service_localsend();
        let picker = app
            .top_dialog()
            .and_then(|d| d.as_any())
            .and_then(|a| a.downcast_ref::<SendDeviceDialog>())
            .expect("the picker");
        if let Some(found) = picker.summary() {
            summary = Some(found);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert_eq!(
        summary,
        Some(Summary {
            files: 3,
            bytes: 300_000 + 9 + 6
        })
    );
    app.stop_device_discovery();
    let _ = std::fs::remove_dir_all(dir);
}
