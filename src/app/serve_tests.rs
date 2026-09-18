use super::*;
use crate::config::{Config, Keymap, Theme};
use crate::input::DialogId;
use crate::serve::Served;

fn app() -> App {
    App::headless(Config::default(), Keymap::builtin(), Theme::blue())
}

/// A folder with one file in it, to share.
fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("hcmd-app-serve-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    std::fs::write(dir.join("hello.txt"), b"hi\n").expect("file");
    dir
}

#[test]
fn nothing_selected_is_a_message_and_not_a_server() {
    let mut app = app();
    app.serve_paths(Vec::new());
    assert!(!app.is_serving());
    assert!(
        app.message
            .as_deref()
            .is_some_and(|m| m.contains("nothing to serve")),
        "{:?}",
        app.message
    );
    // A filesystem root has no name to serve under, so it is nothing too.
    app.serve_paths(vec![PathBuf::from("/")]);
    assert!(
        app.message
            .as_deref()
            .is_some_and(|m| m.contains("nothing to serve"))
    );
}

#[test]
fn the_share_lives_exactly_as_long_as_its_dialog() {
    let dir = scratch("life");
    let mut app = app();
    let (tx, mut rx) = mpsc::channel(crate::serve::SERVE_CHANNEL_DEPTH);

    // The keystroke queues; nothing listens until the loop services it.
    app.serve_paths(vec![dir.clone()]);
    assert!(!app.is_serving());
    app.service_serve(&tx);
    assert!(app.is_serving(), "the loop started the listener");
    assert_eq!(
        app.top_dialog().map(crate::dialog::Dialog::id),
        Some(DialogId::Serve),
        "and put the dialog up"
    );
    // The dialog names a localhost URL with the live port, and it answers.
    let url = app
        .top_dialog()
        .and_then(|d| d.as_any())
        .and_then(|a| a.downcast_ref::<crate::ui::dialog::serve::ServeDialog>())
        .and_then(|d| d.urls().iter().find(|u| u.contains("localhost")).cloned())
        .expect("a localhost address");
    let page = crate::net::get_text(&url).expect("the index answers");
    assert!(
        page.contains("hello.txt")
            || page.contains(dir.file_name().and_then(|n| n.to_str()).unwrap_or("")),
        "{page}"
    );

    // The request the fetch made reaches the dialog's log through the loop.
    let event = rx.try_recv().expect("the listener reported the request");
    app.apply_serve_event(event);
    let log = app
        .top_dialog()
        .and_then(|d| d.as_any())
        .and_then(|a| a.downcast_ref::<crate::ui::dialog::serve::ServeDialog>())
        .map(|d| d.log().join("\n"))
        .unwrap_or_default();
    assert!(log.contains("GET"), "{log}");
    assert!(log.contains("200"), "{log}");

    // A second Ctrl+N while serving is refused rather than doubled up.
    app.serve_paths(vec![dir.clone()]);
    assert!(
        app.message
            .as_deref()
            .is_some_and(|m| m.contains("already serving"))
    );

    // Closing the dialog stops the listener.
    app.close_dialogs();
    app.stop_serving();
    assert!(!app.is_serving());
    assert!(
        crate::net::get_text(&url).is_err(),
        "the port is closed once the dialog is gone"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_log_keeps_the_last_ten_requests() {
    let mut dialog =
        crate::ui::dialog::serve::ServeDialog::new(vec!["http://localhost:1/".into()], 1);
    for i in 0..12_u16 {
        dialog.push(&Served {
            peer: "10.0.0.7".into(),
            method: "GET".into(),
            path: format!("/f{i}"),
            status: 200,
            bytes: 1024,
        });
    }
    let log = dialog.log();
    assert_eq!(log.len(), crate::ui::dialog::serve::LOG_ROWS);
    // Each row starts with the time it was answered, HH:MM:SS.
    let stamp: Vec<char> = log
        .first()
        .map(|l| l.chars().take(9).collect())
        .unwrap_or_default();
    assert_eq!(stamp.len(), 9, "{log:?}");
    assert!(
        stamp.get(2) == Some(&':') && stamp.get(5) == Some(&':') && stamp.get(8) == Some(&' '),
        "{log:?}"
    );
    assert!(
        stamp
            .iter()
            .enumerate()
            .all(|(i, c)| matches!(i, 2 | 5 | 8) || c.is_ascii_digit()),
        "{log:?}"
    );
    assert!(log.first().is_some_and(|l| l.contains("/f2")), "{log:?}");
    assert!(log.last().is_some_and(|l| l.contains("/f11")), "{log:?}");
    assert!(
        log.last()
            .is_some_and(|l| l.contains("1.0 KB") && l.contains("10.0.0.7")),
        "{log:?}"
    );
}
