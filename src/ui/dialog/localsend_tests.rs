//! The picker: the list, the fields, the answer.

use super::*;
use crate::input::{KeyModifiers, KeyPress};

fn key(code: KeyCode) -> DialogKey {
    DialogKey::raw(KeyPress::new(code, KeyModifiers::NONE))
}

fn typed(c: char) -> DialogKey {
    key(KeyCode::Char(c))
}

fn peer(alias: &str, host: &str) -> Peer {
    Peer {
        alias: alias.into(),
        host: host.into(),
        port: 53317,
        protocol: Protocol::Https,
        fingerprint: Some(format!("fp-{alias}")),
        device_type: Some(DeviceType::Mobile),
        model: Some("Pixel".into()),
    }
}

#[test]
fn enter_with_nobody_heard_refuses_and_a_device_that_arrives_is_chosen() {
    let mut d = SendDeviceDialog::new(Selection {
        folders: 1,
        files: 1,
    });
    assert_eq!(d.title(), "Send folder and 1 file - counting...");
    assert!(matches!(
        d.handle_key(&key(KeyCode::Enter)),
        DialogOutcome::Consumed
    ));
    assert!(
        d.refusal
            .as_deref()
            .is_some_and(|r| r.contains("no device yet"))
    );
    d.set_peers(vec![peer("Zed", "10.0.0.2"), peer("Amy", "10.0.0.1")]);
    match d.handle_key(&key(KeyCode::Enter)) {
        DialogOutcome::Accept(DialogResult::SendTo(choice)) => {
            assert_eq!(choice.peer.alias, "Zed", "the first row, as given");
            assert_eq!(choice.peer.fingerprint.as_deref(), Some("fp-Zed"));
            assert_eq!(choice.pin, None, "nothing typed is no PIN");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn arrows_and_letters_move_the_cursor_and_a_refresh_keeps_it_on_the_same_device() {
    let mut d = SendDeviceDialog::new(Selection {
        folders: 0,
        files: 1,
    });
    d.set_peers(vec![peer("Amy", "1"), peer("Bob", "2"), peer("Cat", "3")]);
    d.handle_key(&key(KeyCode::Down));
    assert_eq!(d.cursor, 1);
    d.handle_key(&typed('c'));
    assert_eq!(
        d.cursor, 2,
        "a letter jumps to the next name starting with it"
    );
    d.handle_key(&typed('a'));
    assert_eq!(d.cursor, 0, "wrapping");
    d.handle_key(&key(KeyCode::Down));
    d.handle_key(&key(KeyCode::Down));
    d.handle_key(&key(KeyCode::Down));
    assert_eq!(d.cursor, 2, "and not past the end");
    // Bob was here; a refresh that lists him elsewhere follows him.
    d.handle_key(&key(KeyCode::Up));
    d.set_peers(vec![peer("Amy", "1"), peer("Dan", "4"), peer("Bob", "2")]);
    assert_eq!(d.peers()[d.cursor].alias, "Bob");
    // A refresh without him lands somewhere valid.
    d.set_peers(vec![peer("Amy", "1")]);
    assert_eq!(d.cursor, 0);
}

#[test]
fn tab_reaches_the_address_and_a_typed_address_outranks_the_list() {
    let mut d = SendDeviceDialog::new(Selection {
        folders: 0,
        files: 1,
    });
    d.set_peers(vec![peer("Amy", "10.0.0.1")]);
    d.handle_key(&key(KeyCode::Tab));
    assert!(d.ring.is(ADDRESS));
    for c in "192.168.1.9:5000".chars() {
        d.handle_key(&typed(c));
    }
    d.handle_key(&key(KeyCode::Tab));
    assert!(d.ring.is(PIN));
    for c in "4321".chars() {
        d.handle_key(&typed(c));
    }
    match d.handle_key(&key(KeyCode::Enter)) {
        DialogOutcome::Accept(DialogResult::SendTo(choice)) => {
            assert_eq!(
                (choice.peer.host.as_str(), choice.peer.port),
                ("192.168.1.9", 5000)
            );
            assert_eq!(choice.peer.fingerprint, None, "typed: nothing to pin");
            assert_eq!(choice.pin.as_deref(), Some("4321"));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_buttons_send_and_cancel_and_esc_cancels_anywhere() {
    let mut d = SendDeviceDialog::new(Selection {
        folders: 0,
        files: 1,
    });
    d.set_peers(vec![peer("Amy", "10.0.0.1")]);
    for _ in 0..3 {
        d.handle_key(&key(KeyCode::Tab));
    }
    assert!(d.ring.is(SEND));
    assert!(matches!(
        d.handle_key(&typed(' ')),
        DialogOutcome::Accept(_)
    ));
    d.handle_key(&key(KeyCode::Tab));
    assert!(d.ring.is(CANCEL));
    // A cancel is an answer of nothing, not a bare `Cancel`: only an answer
    // reaches the application's `(SendDevice, None)` arm, which is what puts
    // discovery and the queued paths away.
    assert!(matches!(
        d.handle_key(&key(KeyCode::Enter)),
        DialogOutcome::Accept(DialogResult::None)
    ));
    assert!(matches!(
        d.handle_key(&typed(' ')),
        DialogOutcome::Accept(DialogResult::None)
    ));
    assert!(matches!(
        d.handle_key(&key(KeyCode::Esc)),
        DialogOutcome::Accept(DialogResult::None)
    ));
    d.handle_key(&key(KeyCode::Tab));
    assert!(d.ring.is(LIST));
    assert!(
        matches!(
            d.handle_key(&key(KeyCode::Esc)),
            DialogOutcome::Accept(DialogResult::None)
        ),
        "Esc anywhere"
    );
}

#[test]
fn the_box_is_a_fixed_size_however_many_devices_arrive() {
    let mut d = SendDeviceDialog::new(Selection {
        folders: 0,
        files: 1,
    });
    let before = d.size_hint();
    d.set_peers(
        (0..40)
            .map(|i| peer(&format!("Device {i}"), &format!("10.0.0.{i}")))
            .collect(),
    );
    assert_eq!(d.size_hint(), before);
}

#[test]
fn a_row_names_the_device_its_kind_model_and_address_and_says_when_it_is_unencrypted() {
    let text = SendDeviceDialog::row_text(&peer("Nice Orange", "10.0.0.7"), 70);
    assert!(text.starts_with("Nice Orange"), "{text}");
    assert!(
        text.contains("mobile") && text.contains("Pixel") && text.contains("10.0.0.7:53317"),
        "{text}"
    );
    assert!(!text.contains("http://"), "encrypted: a bare address");
    let mut plain = peer("Old", "10.0.0.8");
    plain.protocol = Protocol::Http;
    assert!(SendDeviceDialog::row_text(&plain, 70).contains("http://10.0.0.8:53317"));
}

#[test]
fn the_selection_is_described_in_words_and_the_count_arrives_later() {
    assert_eq!(
        Selection {
            folders: 1,
            files: 0
        }
        .describe(),
        "folder"
    );
    assert_eq!(
        Selection {
            folders: 0,
            files: 3
        }
        .describe(),
        "3 files"
    );
    assert_eq!(
        Selection {
            folders: 2,
            files: 1
        }
        .describe(),
        "2 folders and 1 file"
    );
    assert_eq!(Selection::default().describe(), "nothing");

    // A folder's title says what it comes to once counted, and the line
    // under the list turns into a warning past the many-files mark.
    let mut d = SendDeviceDialog::new(Selection {
        folders: 1,
        files: 0,
    });
    assert_eq!(d.title(), "Send folder - counting...");
    assert_eq!(
        d.sending_line(),
        ("Counting the files...".to_string(), false)
    );
    d.set_summary(Summary {
        files: 12,
        bytes: 4_200_000,
    });
    assert_eq!(d.title(), "Send folder - 12 files");
    let (line, warn) = d.sending_line();
    assert_eq!(line, "12 files, 4.0 MB in all");
    assert!(!warn);
    d.set_summary(Summary {
        files: 1234,
        bytes: 2_400_000_000,
    });
    assert_eq!(d.title(), "Send folder - 1234 files");
    let (line, warn) = d.sending_line();
    assert!(line.starts_with("Warning: 1234 files, "), "{line}");
    assert!(line.contains("more than 50 files"), "{line}");
    assert!(warn, "drawn as a warning");
    // Several folders count together.
    let mut d = SendDeviceDialog::new(Selection {
        folders: 2,
        files: 0,
    });
    d.set_summary(Summary {
        files: 40,
        bytes: 1,
    });
    assert_eq!(d.title(), "Send 2 folders - 40 files");
    // Files alone are named by their count up front.
    let d = SendDeviceDialog::new(Selection {
        folders: 0,
        files: 2,
    });
    assert_eq!(d.title(), "Send 2 files");
    let one = SendDeviceDialog::new(Selection {
        folders: 0,
        files: 1,
    });
    assert_eq!(one.title(), "Send 1 file");
    // And the box does not grow for the line.
    let before = d.size_hint();
    let mut d = d;
    d.set_summary(Summary {
        files: 2,
        bytes: 10,
    });
    assert_eq!(d.size_hint(), before);
}

#[test]
fn alt_s_sends_and_alt_n_cancels_from_anywhere_in_the_box() {
    use crate::input::KeyModifiers;
    let alt = |c: char| DialogKey::raw(KeyPress::new(KeyCode::Char(c), KeyModifiers::ALT));
    let mut d = SendDeviceDialog::new(Selection {
        folders: 0,
        files: 1,
    });
    d.set_peers(vec![peer("Amy", "10.0.0.1")]);
    assert_eq!(d.mnemonic_letters(), vec!['s', 'n']);
    d.handle_key(&key(KeyCode::Tab));
    assert!(d.ring.is(ADDRESS), "focus in a field");
    assert!(matches!(
        d.handle_key(&alt('s')),
        DialogOutcome::Accept(DialogResult::SendTo(_))
    ));
    assert!(matches!(
        d.handle_key(&alt('n')),
        DialogOutcome::Accept(DialogResult::None)
    ));
}

#[test]
fn a_refusal_takes_the_heading_row_and_a_device_arriving_clears_it() {
    let mut d = SendDeviceDialog::new(Selection {
        folders: 0,
        files: 1,
    });
    d.handle_key(&key(KeyCode::Enter));
    assert!(d.refusal.is_some(), "nothing to send to yet");
    d.set_peers(vec![peer("Amy", "10.0.0.1")]);
    assert!(d.refusal.is_none(), "a device answers the refusal");
    // The box has no row of its own for it: heading, list, rule, sending,
    // two fields, buttons, border.
    assert_eq!(d.size_hint().1, 1 + LIST_ROWS + 1 + 1 + 2 + 1 + 2);
}
