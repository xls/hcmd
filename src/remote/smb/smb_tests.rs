//! [`SmbFs`] against [`FakeOps`]: the whole share-splitting rule, the server
//! root, the refusals and the `Vfs` surface, with no server anywhere.

use super::fake::FakeOps;
use super::*;
use crate::remote::fs::RemoteFs;
use crate::remote::{Protocol, RemoteId};
use crate::vfs::{EntryKind, Vfs};
use std::io::{Read, Seek, SeekFrom, Write};
use std::time::Duration;

fn target() -> Target {
    Target {
        protocol: Protocol::Smb,
        host: "nas.local".to_string(),
        port: 445,
        user: "thorin".to_string(),
        dir: Some("/Media".to_string()),
    }
}

/// A server with two shares and a small tree on the first.
fn server() -> FakeOps {
    FakeOps::new()
        .with_share("Media")
        .with_share("Backups")
        .with_dir("Media", "Photos")
        .with_file("Media", "Photos/beach.jpg", b"jpegjpegjpeg")
        .with_file("Media", "notes.txt", b"hello share")
}

fn fs_over(ops: FakeOps) -> (Arc<SmbFs>, Arc<FakeOps>) {
    let ops = Arc::new(ops);
    let fs = SmbFs::new(
        target(),
        Arc::clone(&ops) as Arc<dyn SmbOps>,
        "/Media".to_string(),
    );
    (fs, ops)
}

fn names(rows: &[Entry]) -> Vec<String> {
    rows.iter().map(|row| row.name.clone()).collect()
}

#[test]
fn the_root_of_a_connection_is_the_server_and_lists_its_shares() {
    let (fs, _ops) = fs_over(server());
    let rows = fs.list("/").expect("lists");
    assert_eq!(
        names(&rows),
        vec!["Media".to_string(), "Backups".to_string()]
    );
    assert!(
        rows.iter().all(|row| row.kind == EntryKind::Dir),
        "a share is entered like a directory"
    );
}

#[test]
fn the_first_path_component_selects_the_share_and_the_rest_is_inside_it() {
    assert_eq!(SmbFs::split("/"), None, "/ is the server");
    assert_eq!(SmbFs::split(""), None);
    assert_eq!(SmbFs::split("/Media"), Some(("Media", "")));
    assert_eq!(SmbFs::split("/Media/"), Some(("Media", "")));
    assert_eq!(SmbFs::split("/Media/Photos"), Some(("Media", "Photos")));
    assert_eq!(
        SmbFs::split("/Media/Photos/2024/"),
        Some(("Media", "Photos/2024"))
    );
}

#[test]
fn a_share_root_lists_the_share_and_a_directory_on_it_lists_the_directory() {
    let (fs, _ops) = fs_over(server());
    let mut top = names(&fs.list("/Media").expect("lists"));
    top.sort();
    assert_eq!(top, vec!["Photos".to_string(), "notes.txt".to_string()]);
    assert_eq!(
        names(&fs.list("/Media/Photos").expect("lists")),
        vec!["beach.jpg".to_string()]
    );
}

#[test]
fn a_share_that_is_not_there_is_not_found_rather_than_empty() {
    let (fs, _ops) = fs_over(server());
    let err = fs.list("/Archive").expect_err("no such share");
    assert!(matches!(err, Error::NotFound(_)), "{err:?}");
}

#[test]
fn stat_answers_for_the_server_for_a_share_and_for_a_file() {
    let (fs, _ops) = fs_over(server());
    let root = fs.stat("/").expect("the server");
    assert_eq!(root.kind, EntryKind::Dir);
    let share = fs.stat("/Media").expect("the share");
    assert_eq!(share.name, "Media");
    assert_eq!(share.kind, EntryKind::Dir);
    let file = fs.stat("/Media/notes.txt").expect("the file");
    assert_eq!(file.name, "notes.txt");
    assert_eq!(file.size, 11);
}

#[test]
fn reading_a_file_on_a_share_seeks() {
    let (fs, _ops) = fs_over(server());
    let mut reader = fs.open_seek("/Media/notes.txt").expect("opens");
    reader.seek(SeekFrom::Start(6)).expect("seeks");
    let mut rest = String::new();
    reader.read_to_string(&mut rest).expect("reads");
    assert_eq!(rest, "share");
    assert!(
        fs.capabilities().seekable,
        "and the capability says so, which is what the viewer reads"
    );
}

#[test]
fn writing_a_file_commits_on_flush() {
    let (fs, ops) = fs_over(server());
    let mut writer = fs.open_write("/Media/new.txt").expect("opens");
    writer.write_all(b"written").expect("writes");
    assert_eq!(ops.bytes("Media", "new.txt"), None, "not yet");
    writer.flush().expect("commits");
    assert_eq!(ops.bytes("Media", "new.txt"), Some(b"written".to_vec()));
}

#[test]
fn the_server_root_is_not_a_place_a_file_can_be_made() {
    let (fs, _ops) = fs_over(server());
    for outcome in [
        fs.open_write("/").map(|_| ()).err(),
        fs.create_dir("/").err(),
        fs.remove_file("/").err(),
        fs.open_read("/").map(|_| ()).err(),
    ] {
        let err = outcome.expect("refused");
        assert!(matches!(err, Error::InvalidPath(_)), "{err:?}");
        assert!(
            err.to_string().contains("the server itself"),
            "the refusal says why: {err}"
        );
    }
}

#[test]
fn a_share_is_refused_as_a_file_rather_than_half_attempted() {
    let (fs, _ops) = fs_over(server());
    let Err(err) = fs.open_write("/Media") else {
        panic!("a share is not a file");
    };
    assert!(matches!(err, Error::InvalidPath(_)), "{err:?}");
    assert!(err.to_string().contains("not a file"), "{err}");
}

#[test]
fn a_rename_inside_a_share_is_server_side_and_one_across_two_is_refused() {
    let (fs, ops) = fs_over(server());
    fs.rename("/Media/notes.txt", "/Media/Photos/notes.txt")
        .expect("renames");
    assert!(!ops.exists("Media", "notes.txt"));
    assert!(ops.exists("Media", "Photos/notes.txt"));

    let err = fs
        .rename("/Media/Photos/notes.txt", "/Backups/notes.txt")
        .expect_err("refused");
    assert!(
        matches!(err, Error::Unsupported("renaming between two shares")),
        "{err:?}"
    );
    assert!(
        ops.exists("Media", "Photos/notes.txt"),
        "and nothing was moved"
    );
}

#[test]
fn no_entry_from_this_backend_is_a_link_and_read_link_refuses() {
    let (fs, _ops) = fs_over(server());
    let rows = fs.list("/Media").expect("lists");
    assert!(!rows.is_empty(), "there is something to check");
    assert!(
        rows.iter()
            .all(|row| !matches!(row.kind, EntryKind::Symlink { .. })),
        "SMB reparse points are not read, so nothing claims to be a link"
    );
    let err = fs.read_link("/Media/notes.txt").expect_err("refused");
    assert!(matches!(err, Error::Unsupported(_)), "{err:?}");
}

#[test]
fn a_lost_connection_is_reported_as_lost_and_stops_being_live() {
    let (fs, _ops) = fs_over(server().drop_connection_at("Media", "notes.txt"));
    assert!(fs.is_live());
    let Err(err) = fs.open_read("/Media/notes.txt") else {
        panic!("the connection dropped");
    };
    assert!(matches!(err, Error::ConnectionLost(_)), "{err:?}");
    assert!(!fs.is_live(), "and the panel can see it");
}

#[test]
fn closing_the_backend_stops_it_and_is_idempotent() {
    let (fs, _ops) = fs_over(server());
    fs.close();
    fs.close();
    assert!(!fs.is_live());
    let err = fs.list("/Media").expect_err("closed");
    assert!(matches!(err, Error::ConnectionLost(_)), "{err:?}");
}

#[test]
fn the_capabilities_are_the_ones_the_ui_asks_before_it_offers_an_operation() {
    let (fs, _ops) = fs_over(server());
    let caps = fs.capabilities();
    assert!(caps.writable);
    assert!(caps.seekable);
    assert!(caps.atomic_rename);
    assert!(caps.has_directories);
    assert!(!caps.can_execute, "a share has no local path to execute");
    assert_eq!(caps.latency, crate::vfs::LatencyClass::Network);
    assert_eq!(fs.protocol(), Protocol::Smb);
}

/// The whole point of the exercise: a connected panel is an ordinary panel.
#[tokio::test]
async fn a_connected_share_is_an_ordinary_vfs_and_its_listing_leads_with_the_parent_row() {
    let (smb, _ops) = fs_over(server());
    let fs = RemoteFs::new(
        target(),
        smb as Arc<dyn RemoteTransport>,
        Duration::from_secs(2),
    );
    fs.adopt(RemoteId(5));

    let mut rx = fs.read_dir(&RemoteId(5).path("/Media"));
    let mut rows = Vec::new();
    while let Some(item) = rx.recv().await {
        rows.push(item.expect("a row"));
    }
    assert_eq!(rows.len(), 3, "the parent row and the two entries");
    assert!(rows.first().is_some_and(|row| row.is_parent));

    // And the server root, one segment up, is the share list.
    let mut rx = fs.read_dir(&RemoteId(5).root());
    let mut shares = Vec::new();
    while let Some(item) = rx.recv().await {
        shares.push(item.expect("a row"));
    }
    assert_eq!(
        shares.iter().filter(|row| !row.is_parent).count(),
        2,
        "two shares"
    );

    // A file on the share reads through the ordinary `Vfs` surface.
    let mut reader = fs
        .open_read(&RemoteId(5).path("/Media/notes.txt"))
        .expect("opens");
    let mut body = String::new();
    reader.read_to_string(&mut body).expect("reads");
    assert_eq!(body, "hello share");
}

#[test]
fn nothing_this_backend_renders_carries_a_credential() {
    let (fs, _ops) = fs_over(server());
    let shown = format!("{fs:?}");
    assert!(!shown.contains("hunter2"), "{shown}");
    assert_eq!(fs.target().authority(), "smb://thorin@nas.local:445");
}
