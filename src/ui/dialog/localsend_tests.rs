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
fn the_choice_round_trips_through_the_encoding_and_junk_does_not() {
    let p = peer("Nice Orange", "10.0.0.7");
    let text = encode_choice(&p, "1234");
    let (back, pin) = decode_choice(&text).expect("decodes");
    assert_eq!(
        (back.alias.as_str(), back.host.as_str(), back.port),
        ("Nice Orange", "10.0.0.7", 53317)
    );
    assert_eq!(back.protocol, Protocol::Https);
    assert_eq!(back.fingerprint.as_deref(), Some("fp-Nice Orange"));
    assert_eq!(pin, "1234");
    let mut plain = p;
    plain.protocol = Protocol::Http;
    plain.fingerprint = None;
    let (back, pin) = decode_choice(&encode_choice(&plain, "")).expect("decodes");
    assert_eq!(
        (back.protocol, back.fingerprint, pin.as_str()),
        (Protocol::Http, None, "")
    );
    assert!(decode_choice("nonsense").is_none());
    assert!(decode_choice("h\tnotaport\thttps\t\tname\t").is_none());
    assert!(decode_choice("h\t1\tgopher\t\tname\t").is_none());
}

#[test]
fn enter_with_nobody_heard_refuses_and_a_device_that_arrives_is_chosen() {
    let mut d = SendDeviceDialog::new(Selection {
        folders: 1,
        files: 1,
    });
    assert_eq!(d.title(), "Send a folder and a file to a device");
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
        DialogOutcome::Accept(DialogResult::Text(text)) => {
            let (chosen, pin) = decode_choice(&text).expect("decodes");
            assert_eq!(chosen.alias, "Zed", "the first row, as given");
            assert_eq!(pin, "");
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
        DialogOutcome::Accept(DialogResult::Text(text)) => {
            let (chosen, pin) = decode_choice(&text).expect("decodes");
            assert_eq!((chosen.host.as_str(), chosen.port), ("192.168.1.9", 5000));
            assert_eq!(chosen.fingerprint, None, "typed: nothing to pin");
            assert_eq!(pin, "4321");
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
        "a folder"
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
        "2 folders and a file"
    );
    assert_eq!(Selection::default().describe(), "nothing");

    let mut d = SendDeviceDialog::new(Selection {
        folders: 1,
        files: 0,
    });
    assert_eq!(d.title(), "Send a folder to a device");
    assert_eq!(d.sending_line(), "Sending a folder: counting...");
    d.set_summary(Summary {
        files: 1234,
        bytes: 2_400_000_000,
    });
    let line = d.sending_line();
    assert!(line.starts_with("Sending a folder: 1234 files, "), "{line}");
    assert!(line.ends_with(" in all"), "{line}");
    // Files alone need no counting to be announced.
    let d = SendDeviceDialog::new(Selection {
        folders: 0,
        files: 2,
    });
    assert_eq!(d.sending_line(), "Sending 2 files");
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
        DialogOutcome::Accept(DialogResult::Text(_))
    ));
    assert!(matches!(
        d.handle_key(&alt('n')),
        DialogOutcome::Accept(DialogResult::None)
    ));
}
