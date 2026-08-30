use super::*;

/// Two elements of a real answer from
/// `api.github.com/repos/xls/hcmd/contents/themes?ref=master`, trimmed of the
/// fields the scan does not read but keeping the ones that could be mistaken
/// for a name: `path` and `download_url` both end in `.toml`.
const REAL: &str = r#"[{"name":"ayu-dark.toml","path":"themes/ayu-dark.toml","sha":"a39aab68","size":2375,"url":"https://api.github.com/repos/xls/hcmd/contents/themes/ayu-dark.toml?ref=master","download_url":"https://raw.githubusercontent.com/xls/hcmd/master/themes/ayu-dark.toml","type":"file","_links":{"self":"https://api.github.com/repos/xls/hcmd/contents/themes/ayu-dark.toml?ref=master","git":"https://api.github.com/repos/xls/hcmd/git/blobs/a39aab68","html":"https://github.com/xls/hcmd/blob/master/themes/ayu-dark.toml"}},{"name":"tokyo-night.toml","path":"themes/tokyo-night.toml","sha":"b1","size":2400,"type":"file","_links":{"self":"x"}}]"#;

#[test]
fn a_real_listing_yields_one_name_per_element() {
    let names = names_in_listing(REAL);
    assert_eq!(names, vec!["ayu-dark", "tokyo-night"], "{names:?}");
}

#[test]
fn the_pretty_printed_form_reads_the_same() {
    // `?pretty` or a hand-saved answer puts a space after the colon and
    // newlines between the fields.
    let pretty = "[\n  {\n    \"name\": \"nord.toml\",\n    \"type\": \"file\"\n  }\n]";
    assert_eq!(names_in_listing(pretty), vec!["nord"]);
}

#[test]
fn anything_that_is_not_a_theme_file_is_ignored() {
    let json = r#"[{"name":"README.md"},{"name":"palette.py"},{"name":"blue.toml"}]"#;
    assert_eq!(names_in_listing(json), vec!["blue"]);
}

#[test]
fn a_name_that_would_escape_the_themes_directory_is_refused() {
    // The names come off the network and are about to be joined onto the
    // configuration directory.
    let json = r#"[{"name":"../../.bashrc.toml"},{"name":"/etc/passwd.toml"},{"name":"ok.toml"}]"#;
    assert_eq!(names_in_listing(json), vec!["ok"]);
}

#[test]
fn a_body_that_is_not_the_listing_yields_nothing_rather_than_nonsense() {
    for body in [
        "",
        "<html>404: Not Found</html>",
        "{\"message\":\"Not Found\"}",
    ] {
        assert!(names_in_listing(body).is_empty(), "{body}");
    }
}

#[test]
fn a_truncated_answer_stops_where_the_bytes_stop() {
    let names = names_in_listing(r#"[{"name":"dracula.toml"},{"name":"gruv"#);
    assert_eq!(names, vec!["dracula"]);
}

#[test]
fn the_same_name_twice_is_offered_once() {
    let json = r#"[{"name":"nord.toml"},{"name":"nord.toml"}]"#;
    assert_eq!(names_in_listing(json), vec!["nord"]);
}

#[test]
fn the_urls_name_the_project_repository() {
    assert_eq!(
        contents_url(),
        "https://api.github.com/repos/xls/hcmd/contents/themes?ref=master"
    );
    assert_eq!(
        theme_url("dracula"),
        "https://raw.githubusercontent.com/xls/hcmd/master/themes/dracula.toml"
    );
}

/// A throwaway configuration directory.
fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("hcmd-catalogue-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

#[test]
fn a_real_theme_is_written_and_loads_back() {
    let dir = temp_dir("write");
    let text = crate::config::builtin_theme("nord").expect("nord ships");
    let path = store_theme_text(&dir, "mine", text).expect("stored");
    assert!(path.is_file());
    assert!(
        crate::config::theme_names_in(Some(&dir))
            .iter()
            .any(|n| n == "mine"),
        "the picker's scan finds what was written"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn what_is_not_a_theme_is_never_written() {
    // The failure this exists for: a captive portal or a 404 page answers 200
    // with HTML, and a file written from it survives the restart and makes
    // the loader fall back to blue with a warning about a file the user never
    // wrote.
    let dir = temp_dir("junk");
    for body in ["<html>404: Not Found</html>", "", "just = \"toml\"\n"] {
        assert!(store_theme_text(&dir, "junk", body).is_err(), "{body}");
    }
    assert!(!dir.join("themes").join("junk.toml").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_theme_that_is_already_here_is_never_fetched() {
    // No network in the test suite, so this passing at all is the assertion:
    // a built-in and a file on disk both return without asking anybody.
    let dir = temp_dir("present");
    let text = crate::config::builtin_theme("nord").expect("nord ships");
    store_theme_text(&dir, "mine", text).expect("stored");
    assert!(ensure_installed_in(&dir, "nord").is_ok());
    assert!(ensure_installed_in(&dir, "mine").is_ok());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_file_beats_the_built_in_of_the_same_name() {
    let dir = temp_dir("override");
    let text = crate::config::builtin_theme("dracula").expect("dracula ships");
    store_theme_text(&dir, "nord", text).expect("stored");
    let theme = installed_theme(Some(&dir), "nord").expect("loaded");
    assert_eq!(theme.name, "dracula", "the file on disk is what was read");
    let plain = installed_theme(None, "nord").expect("loaded");
    assert_eq!(plain.name, "nord");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
#[ignore = "asks GitHub; run with: cargo test -- --ignored the_live_repository"]
fn the_live_repository_answers_with_its_themes() {
    // The one thing no offline test can check: that the URL is right and the
    // scan reads what the server actually sends today.
    let names = fetch_names().expect("the contents API answered");
    assert!(names.len() >= 20, "{names:?}");
    for shipped in crate::config::theme_names() {
        assert!(
            names.iter().any(|n| n == shipped),
            "{shipped} is in the repository: {names:?}"
        );
    }
}
