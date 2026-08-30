//! The parts of connecting that do not need a socket: the identity, the
//! credential-free logins, and where a panel opens.

use super::*;

#[test]
fn a_domain_is_spelled_with_a_backslash_and_the_last_one_splits() {
    assert_eq!(split_identity("thorin"), ("", "thorin"));
    assert_eq!(split_identity("CORP\\thorin"), ("CORP", "thorin"));
    assert_eq!(split_identity("WORKGROUP\\guest"), ("WORKGROUP", "guest"));
    // A backslash inside the name is part of the name; the last one is the
    // separator, so the domain is never the longer half by accident.
    assert_eq!(split_identity("a\\b\\c"), ("a\\b", "c"));
    assert_eq!(split_identity(""), ("", ""));
}

#[test]
fn guest_anonymous_and_an_empty_name_are_logins_with_no_credential() {
    for user in ["", "guest", "Guest", "GUEST", "anonymous", "Anonymous"] {
        assert!(is_guest(user), "{user:?} needs no credential");
    }
    for user in ["thorin", "CORP\\thorin", "guestuser"] {
        assert!(!is_guest(user), "{user:?} does");
    }
    // The domain half does not change the answer: a guest session has no
    // domain to be in.
    assert!(is_guest("WORKGROUP\\guest"));
}

#[test]
fn a_line_that_named_no_share_opens_on_the_server_where_the_shares_are() {
    assert_eq!(start_dir(None), "/");
    assert_eq!(start_dir(Some("")), "/");
    assert_eq!(start_dir(Some("/")), "/");
    assert_eq!(start_dir(Some("/Media")), "/Media");
    assert_eq!(start_dir(Some("Media")), "/Media");
    assert_eq!(start_dir(Some("/Media/Photos/")), "/Media/Photos");
}

#[test]
fn the_auth_plan_for_a_guest_line_has_no_method_in_it() {
    use crate::remote::auth::AuthPlan;
    use crate::remote::url;

    let home = std::path::PathBuf::from("/nonexistent");
    let parsed = url::parse("//nas.local/Media", Protocol::Sftp, "thorin").expect("parses");
    assert_eq!(parsed.target.protocol, Protocol::Smb);
    assert!(
        AuthPlan::for_line(&parsed, &home).methods().is_empty(),
        "a guest share is opened, not interrogated"
    );

    let named =
        url::parse("smb://CORP\\thorin@nas.local/Media", Protocol::Sftp, "x").expect("parses");
    assert_eq!(named.target.user, "CORP\\thorin");
    assert_eq!(
        AuthPlan::for_line(&named, &home).methods().len(),
        1,
        "a named account is asked for a password and nothing else"
    );
}

#[test]
fn a_password_typed_on_an_smb_line_never_reaches_anything_that_remembers() {
    use crate::remote::url;

    let line = "smb://thorin:hunter2@nas.local/Media";
    let parsed = url::parse(line, Protocol::Sftp, "x").expect("parses");
    assert_eq!(
        parsed
            .password
            .as_ref()
            .and_then(crate::remote::secret::Secret::expose_str),
        Some("hunter2")
    );
    assert!(!format!("{parsed:?}").contains("hunter2"));
    assert!(!parsed.target.authority().contains("hunter2"));
    assert_eq!(url::redact(line), "smb://thorin@nas.local/Media");
    // The UNC spelling redacts too, which it did not when its leading `//`
    // was read as the start of the path instead of as the scheme.
    assert_eq!(
        url::redact("//thorin:hunter2@nas.local/Media"),
        "//thorin@nas.local/Media"
    );
    assert_eq!(
        url::redact("\\\\thorin:hunter2@nas.local\\Media"),
        "\\\\thorin@nas.local\\Media"
    );
}
