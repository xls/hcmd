//! SMB end to end, against a real server and nothing else.
//!
//! # This is not part of `cargo test`, deliberately
//!
//! Every test here is `#[ignore]` **and** refuses to run unless
//! `HCMD_SMB_TEST=1` is set, for the reasons `remote_sshd.rs` gives: a
//! contributor's `cargo test` must not open a socket behind their back, and a
//! machine with no server must not fail the gate.
//!
//! Unlike `sshd`, a Samba server cannot be started non-root on a port the
//! kernel picked - SMB is a fixed-port protocol and `smbd` wants a config
//! file, a state directory and usually a privileged bind - so this suite does
//! not start one. It connects to a server the operator names:
//!
//! ```text
//! HCMD_SMB_TEST=1 \
//! HCMD_SMB_HOST=127.0.0.1 \
//! HCMD_SMB_SHARE=scratch \
//! HCMD_SMB_USER=thorin \
//! HCMD_SMB_PASS=hunter2 \
//! cargo test --test remote_smb -- --ignored --test-threads=1
//! ```
//!
//! `HCMD_SMB_PORT` defaults to 445 and `HCMD_SMB_USER` to `guest`, which is
//! the anonymous case. Everything the suite writes goes under one directory
//! named after the process, and it removes it on the way out.
//!
//! What it covers: connect, the share list, a directory listing, an upload, a
//! download, a seek, a rename, and a delete - which is the same list the SFTP
//! suite covers, through the same `Vfs` surface.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "an integration test is its own crate, so it is not #[cfg(test)] \
              and clippy.toml's allow-*-in-tests keys do not reach it. \
              Panicking assertions are the point of a test."
)]

use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::Arc;

use holoscommander::config::RemoteConfig;
use holoscommander::remote::connect::ConnectId;
use holoscommander::remote::prompter::Prompter;
use holoscommander::remote::secret::Secret;
use holoscommander::remote::smb::SmbFs;
use holoscommander::remote::transport::RemoteTransport;
use holoscommander::remote::{Protocol, Target, auth::AuthPlan, keyring};

/// Whether the operator asked for this suite.
fn enabled() -> bool {
    std::env::var("HCMD_SMB_TEST").as_deref() == Ok("1")
}

/// The target the operator named.
fn target() -> Target {
    let user = std::env::var("HCMD_SMB_USER").unwrap_or_else(|_| "guest".to_string());
    Target {
        protocol: Protocol::Smb,
        host: std::env::var("HCMD_SMB_HOST").unwrap_or_else(|_| "127.0.0.1".to_string()),
        port: std::env::var("HCMD_SMB_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(445),
        user,
        dir: None,
    }
}

/// The share the operator named.
fn share() -> String {
    std::env::var("HCMD_SMB_SHARE").unwrap_or_else(|_| "scratch".to_string())
}

/// Connect, with the password from the environment and no dialog anywhere: the
/// hooks' event channel is dropped, so an attempt that wanted to ask a
/// question fails instead of hanging.
async fn connect() -> Arc<SmbFs> {
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    drop(rx);
    let password = std::env::var("HCMD_SMB_PASS")
        .ok()
        .as_deref()
        .map(Secret::from_str);
    let plan = AuthPlan::for_password_login(None);
    holoscommander::remote::smb::connect(
        target(),
        plan,
        password,
        &RemoteConfig::default(),
        keyring::store(),
        Prompter::to_loop(tx, ConnectId(1)),
    )
    .await
    .expect("connects to the server named in the environment")
}

/// A directory of this suite's own, under the share's root.
fn scratch() -> String {
    format!("/{}/hcmd-smb-{}", share(), std::process::id())
}

#[tokio::test]
#[ignore = "needs a real SMB server; set HCMD_SMB_TEST=1"]
async fn the_share_list_the_round_trip_and_the_clean_up() {
    if !enabled() {
        return;
    }
    let fs = connect().await;
    let fs = Arc::clone(&fs);

    // Everything below is blocking, which is where a transport method has to
    // be called from.
    tokio::task::spawn_blocking(move || {
        // The server root lists shares, and the one we were told about is
        // among them.
        let shares = fs.list("/").expect("lists the shares");
        assert!(
            shares.iter().any(|row| row.name == share()),
            "{} is not among {:?}",
            share(),
            shares.iter().map(|r| &r.name).collect::<Vec<_>>()
        );

        let dir = scratch();
        fs.create_dir(&dir).expect("makes its own directory");

        // Upload, and the flush is the commit.
        let file = format!("{dir}/hello.txt");
        let body = b"hello from holos commander";
        let mut writer = fs.open_write(&file).expect("opens for writing");
        writer.write_all(body).expect("writes");
        writer.flush().expect("commits");
        drop(writer);

        // It is in the listing, with its size.
        let rows = fs.list(&dir).expect("lists");
        let row = rows
            .iter()
            .find(|row| row.name == "hello.txt")
            .expect("the file is in the listing");
        assert_eq!(row.size, body.len() as u64);

        // Download, and seek, which is what the viewer does.
        let mut reader = fs.open_seek(&file).expect("opens for reading");
        let mut all = Vec::new();
        reader.read_to_end(&mut all).expect("reads");
        assert_eq!(all, body);
        reader.seek(SeekFrom::Start(6)).expect("seeks");
        let mut tail = String::new();
        reader.read_to_string(&mut tail).expect("reads the tail");
        assert_eq!(tail, "from holos commander");
        drop(reader);

        // Rename, server side.
        let renamed = format!("{dir}/renamed.txt");
        fs.rename(&file, &renamed).expect("renames");
        assert!(fs.stat(&file).is_err(), "the old name is gone");
        assert_eq!(
            fs.stat(&renamed).expect("the new name").size,
            body.len() as u64
        );

        // And clean up after itself.
        fs.remove_file(&renamed).expect("deletes the file");
        fs.remove_dir(&dir).expect("deletes the directory");
        assert!(fs.stat(&dir).is_err(), "nothing is left behind");

        fs.close();
    })
    .await
    .expect("the blocking half finishes");
}
