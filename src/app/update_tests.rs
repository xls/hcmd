use super::*;

/// A trimmed but otherwise real answer from
/// `api.github.com/repos/xls/hcmd/releases/latest`: the field order is
/// GitHub's own, and the body carries prose after it.
const RELEASE_JSON: &str = r#"{
  "url": "https://api.github.com/repos/xls/hcmd/releases/207234131",
  "assets_url": "https://api.github.com/repos/xls/hcmd/releases/207234131/assets",
  "html_url": "https://github.com/xls/hcmd/releases/tag/v0.1.1",
  "id": 207234131,
  "tag_name": "v0.1.1",
  "target_commitish": "master",
  "name": "v0.1.1",
  "draft": false,
  "prerelease": false,
  "created_at": "2026-08-29T09:12:44Z",
  "assets": [
    { "name": "hcmd-0.1.1-x86_64-unknown-linux-gnu.tar.gz", "size": 9218331 }
  ],
  "body": "Fixes. The tag_name of the previous release was v0.1.0."
}"#;

#[test]
fn the_tag_is_read_out_of_a_real_release_answer() {
    assert_eq!(tag_from_release(RELEASE_JSON), Some("v0.1.1".to_string()));
}

#[test]
fn spacing_around_the_field_does_not_matter() {
    assert_eq!(
        tag_from_release(r#"{"tag_name"   :    "0.2.0"}"#),
        Some("0.2.0".to_string())
    );
    assert_eq!(
        tag_from_release("{\n\t\"tag_name\": \"v1.10.3-rc1\"\n}"),
        Some("v1.10.3-rc1".to_string())
    );
}

#[test]
fn an_answer_with_no_release_in_it_yields_nothing() {
    // What a repository with no releases actually answers with.
    assert_eq!(
        tag_from_release(r#"{"message":"Not Found","status":"404"}"#),
        None
    );
    assert_eq!(tag_from_release(""), None);
    assert_eq!(tag_from_release(r#"{"tag_name": ""}"#), None);
    assert_eq!(tag_from_release(r#"{"tag_name": 3}"#), None);
    // A tag is refused rather than carried into a status line and a file.
    assert_eq!(tag_from_release(r#"{"tag_name": "v1 \" rm -rf"}"#), None);
}

#[test]
fn versions_compare_as_numbers_and_not_as_text() {
    assert!(is_newer("0.9.0", "0.10.0"), "0.10.0 is after 0.9.0");
    assert!(!is_newer("0.10.0", "0.9.0"));
    assert!(is_newer("0.1.0", "v0.1.1"), "the tag's v is not a version");
    assert!(is_newer("0.1.0", "1.0.0"));
    assert!(!is_newer("0.1.0", "0.1.0"));
    assert!(!is_newer("0.1.0", "v0.1.0"));
    // A missing component is zero, so these are the same version.
    assert!(!is_newer("0.2.0", "0.2"));
    assert!(!is_newer("0.2", "0.2.0"));
    assert!(is_newer("0.2", "0.2.1"));
    // A pre-release suffix reads as the release it is heading for rather than
    // making the whole comparison fail.
    assert!(is_newer("0.1.0", "0.2.0-rc1"));
    assert!(!is_newer("0.2.0", "0.2.0-rc1"));
    // Nonsense is not newer than anything.
    assert!(!is_newer("0.1.0", "nightly"));
}

#[test]
fn nothing_is_announced_twice() {
    // Never seen before: say so.
    assert!(should_notify("0.1.0", "v0.1.1", None));
    // Said once already: not again.
    assert!(!should_notify("0.1.0", "v0.1.1", Some("v0.1.1")));
    // And the file's spelling of it does not decide that.
    assert!(!should_notify("0.1.0", "v0.1.1", Some("0.1.1")));
    // A later release than the one announced is announced in its turn.
    assert!(should_notify("0.1.0", "v0.2.0", Some("v0.1.1")));
    assert!(should_notify("0.1.0", "v0.10.0", Some("v0.9.0")));
    // Equal and older are nothing to report, whatever is in the file.
    assert!(!should_notify("0.1.0", "v0.1.0", None));
    assert!(!should_notify("0.2.0", "v0.1.1", None));
    assert!(!should_notify("0.2.0", "v0.1.1", Some("v0.1.1")));
}

#[test]
fn the_remembered_version_survives_a_round_trip_through_the_file_format() {
    let text = "# Written by hcmd.\nacknowledged = \"v0.1.1\"\n";
    assert_eq!(parse_acknowledged(text), Some("v0.1.1".to_string()));
    // A commented-out key is documentation, not a value.
    assert_eq!(parse_acknowledged("# acknowledged = \"v9.9.9\"\n"), None);
    assert_eq!(parse_acknowledged(""), None);
    assert_eq!(parse_acknowledged("acknowledged = \"\"\n"), None);
    assert_eq!(parse_acknowledged("something_else = \"v2\"\n"), None);
}

#[test]
fn the_message_names_the_version_and_the_command_that_installs_it() {
    let said = notice("v0.1.1");
    assert!(said.contains("v0.1.1"), "{said}");
    assert!(
        said.contains(
            "curl -fsSL https://raw.githubusercontent.com/xls/hcmd/master/install.sh | sh"
        ),
        "{said}"
    );
}

#[test]
fn the_key_queues_one_check_and_not_two() {
    let mut app = App::headless(
        crate::config::Config::default(),
        crate::config::Keymap::builtin(),
        crate::config::Theme::blue(),
    );
    assert_eq!(app.update_check, UpdateCheck::Idle);
    app.request_update_check();
    assert_eq!(app.update_check, UpdateCheck::Queued);
    app.request_update_check();
    assert_eq!(app.update_check, UpdateCheck::Queued);
    app.apply_update_event(UpdateEvent::Newer("v0.1.1".to_string()));
    assert_eq!(app.update_check, UpdateCheck::Idle);
    let said = app.message.clone().unwrap_or_default();
    assert!(said.contains("v0.1.1"), "{said}");
    assert!(said.contains("install.sh"), "{said}");
    // A failure is one status line and nothing else.
    app.apply_update_event(UpdateEvent::Failed("no route to host".to_string()));
    assert_eq!(app.message.as_deref(), Some("no route to host"));
    assert_eq!(app.update_check, UpdateCheck::Idle);
}

#[test]
fn the_file_is_written_once_and_the_second_check_has_nothing_to_say() {
    // The whole once-per-version rule against a real file, which is what the
    // program does between the announcement and the next keystroke.
    let dir = std::env::temp_dir().join(format!("hcmd-update-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    // Nothing announced yet, so a newer release is announced.
    assert_eq!(
        acknowledged_in(&dir).expect("a missing file is not an error"),
        None
    );
    assert!(should_notify("0.1.0", "v0.1.1", None));

    // Showing it remembers it.
    acknowledge_in(&dir, "v0.1.1").expect("the file is written");
    let acked = acknowledged_in(&dir).expect("the file is read");
    assert_eq!(acked.as_deref(), Some("v0.1.1"));
    // And what was written is a TOML file with the one key in it.
    let text = std::fs::read_to_string(dir.join(UPDATE_FILE)).expect("the file is there");
    assert!(text.contains("acknowledged = \"v0.1.1\""), "{text}");

    // The same release, asked again: nothing to say.
    assert!(!should_notify("0.1.0", "v0.1.1", acked.as_deref()));
    // The next one after it is announced in its turn, and replaces the note.
    assert!(should_notify("0.1.0", "v0.2.0", acked.as_deref()));
    acknowledge_in(&dir, "v0.2.0").expect("the file is rewritten");
    assert_eq!(
        acknowledged_in(&dir).expect("the file is read").as_deref(),
        Some("v0.2.0")
    );
    assert!(!should_notify("0.1.0", "v0.2.0", Some("v0.2.0")));

    let _ = std::fs::remove_dir_all(&dir);
}
