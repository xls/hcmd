//! SFTP end to end, against an OpenSSH server on `127.0.0.1` and nothing else.
//!
//!
//! # This is not part of `cargo test`, deliberately
//!
//! Every test here is `#[ignore]` **and** refuses to run unless
//! `HCMD_SSHD_TEST=1` is set. Three reasons, in order:
//!
//! 1. Starting a daemon and binding a port is not something a contributor's
//!    `cargo test` should do behind their back.
//! 2. The loopback bind is the only network operation in this milestone's
//!    entire test suite. **No test in this repository connects to any host
//!    that is not `127.0.0.1`.**
//! 3. A machine with no `sshd` installed must not fail the gate.
//!
//! Run it with:
//!
//! ```text
//! HCMD_SSHD_TEST=1 cargo test --test remote_sshd -- --ignored --test-threads=1
//! ```
//!
//! What it covers, which is the list the design asks for:
//! connect, `known_hosts` learn and then `Known` on the second connect, a
//! listing, a download, an upload, a rename, a delete, and a dropped
//! connection.
//!
//! # The server
//!
//! A **non-root** `sshd` with a throwaway host key, a throwaway user key,
//! `StrictModes no` and the standard `sftp-server` subsystem, on a port the
//! kernel picked. It logs in as whoever is running the tests, because a
//! non-root `sshd` cannot become anybody else - which is exactly the property
//! that makes this safe to run.
//!
//! Nothing here shells out to `ssh` or `sftp`: the client half is
//! `crate::remote::sftp` and no subprocess of this test speaks the client side
//! of the protocol.

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
use std::net::TcpListener;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use holoscommander::app::{App, RemoteEvent, VfsEvent, stream_read};
use holoscommander::config::ArchiveConfig;
use holoscommander::config::RemoteConfig;
use holoscommander::config::{Config, Keymap, Theme};
use holoscommander::dialog::ConnectAnswer;
use holoscommander::dialog::SecretAnswer;
use holoscommander::error::Error;
use holoscommander::panel::{Side, VirtualKind};
use holoscommander::remote::auth::{AuthPlan, SecretKind};
use holoscommander::remote::connect::ConnectId;
use holoscommander::remote::hosts::{AuthMethod, SavedHost};
use holoscommander::remote::keyring::NoKeyring;
use holoscommander::remote::secret::Secret;
use holoscommander::remote::sftp::{ConnectHooks, SftpFs};
use holoscommander::remote::transport::RemoteTransport;
use holoscommander::remote::{Protocol, RemoteFs, Target};
use holoscommander::search::Query;
use holoscommander::vfs::Vfs;
use holoscommander::vfs::VfsRouter;
use tokio::sync::mpsc;

/// The one switch that lets any of this run.
fn enabled() -> bool {
    std::env::var("HCMD_SSHD_TEST").as_deref() == Ok("1")
}

/// A scratch directory of our own, the way `tests/archive_durability.rs` does
/// it: no `tempfile` dependency for something this small.
fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "hcmd-sshd-{tag}-{}-{}",
        std::process::id(),
        Instant::now().elapsed().as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch directory");
    dir
}

/// A port the kernel is willing to give us, released immediately.
///
/// Racy in principle and not in practice: nothing else on a test machine is
/// binding ports in the ephemeral range in the microsecond between the close
/// and `sshd`'s bind, and a collision is a failed test rather than a wrong
/// answer.
fn free_port(listen: &str) -> u16 {
    let listener = TcpListener::bind((listen, 0)).expect("a loopback port");
    listener.local_addr().expect("the address").port()
}

/// A throwaway OpenSSH server, killed when this goes out of scope.
struct Sshd {
    dir: PathBuf,
    /// The loopback address it listens on: `127.0.0.1`, or `::1` for the
    /// IPv6 test.
    listen: String,
    port: u16,
    child: Child,
    key: PathBuf,
    known_hosts: PathBuf,
    home: PathBuf,
    serving: PathBuf,
}

impl Sshd {
    /// Generate the keys, write the configuration, start the daemon and wait
    /// for it to listen on IPv4 loopback.
    fn start(tag: &str) -> Option<Self> {
        Self::start_on(tag, "127.0.0.1")
    }

    /// [`Sshd::start`] on a named loopback address, so the IPv6 test can have
    /// a server on `::1`. `None` when this machine has no such loopback, which
    /// is a skip and not a failure.
    fn start_on(tag: &str, listen: &str) -> Option<Self> {
        if !Path::new("/usr/bin/sshd").exists() || !Path::new("/usr/lib/ssh/sftp-server").exists() {
            return None;
        }
        TcpListener::bind((listen, 0)).ok()?;
        let dir = scratch(tag);
        let host_key = dir.join("host_ed25519");
        let user_key = dir.join("user_ed25519");
        keygen(&host_key)?;
        keygen(&user_key)?;
        let authorized = dir.join("authorized_keys");
        std::fs::copy(user_key.with_extension("pub"), &authorized).expect("authorized_keys");
        let serving = dir.join("served");
        std::fs::create_dir_all(&serving).expect("the served directory");

        let port = free_port(listen);
        let config = dir.join("sshd_config");
        std::fs::write(
            &config,
            format!(
                "Port {port}\n\
                 ListenAddress {listen}\n\
                 HostKey {host}\n\
                 PidFile none\n\
                 AuthorizedKeysFile {authorized}\n\
                 StrictModes no\n\
                 UsePAM no\n\
                 PasswordAuthentication no\n\
                 KbdInteractiveAuthentication no\n\
                 PubkeyAuthentication yes\n\
                 PermitRootLogin no\n\
                 Subsystem sftp /usr/lib/ssh/sftp-server\n\
                 LogLevel ERROR\n",
                host = host_key.display(),
                authorized = authorized.display(),
            ),
        )
        .expect("sshd_config");

        // Its own process group, so that killing it kills the per-connection
        // child `sshd` forks as well. Killing only the listener leaves the
        // established session alive and the "dropped connection" test would
        // pass by not testing anything.
        let child = Command::new("/usr/bin/sshd")
            .arg("-f")
            .arg(&config)
            .arg("-D")
            .arg("-e")
            .process_group(0)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;

        // Wait for the listener rather than sleeping a fixed amount.
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if std::net::TcpStream::connect((listen, port)).is_ok() {
                let home = dir.join("home");
                std::fs::create_dir_all(home.join(".ssh")).expect("a home");
                return Some(Self {
                    known_hosts: home.join(".ssh").join("known_hosts"),
                    home,
                    dir,
                    listen: listen.to_string(),
                    port,
                    child,
                    key: user_key,
                    serving,
                });
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        None
    }

    /// Where the tests put files, on the server's own filesystem.
    fn remote_dir(&self) -> String {
        self.serving.to_string_lossy().into_owned()
    }

    /// Kill the daemon and start another one on the same port with a
    /// **different** host key, which is what a man in the middle looks like.
    ///
    fn restart_with_a_new_host_key(&mut self) -> bool {
        self.kill();
        let host_key = self.dir.join("host_ed25519");
        let _ = std::fs::remove_file(&host_key);
        let _ = std::fs::remove_file(host_key.with_extension("pub"));
        if keygen(&host_key).is_none() {
            return false;
        }
        let config = self.dir.join("sshd_config");
        let Ok(child) = Command::new("/usr/bin/sshd")
            .arg("-f")
            .arg(&config)
            .arg("-D")
            .arg("-e")
            .process_group(0)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            return false;
        };
        self.child = child;
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if std::net::TcpStream::connect((self.listen.as_str(), self.port)).is_ok() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    /// Kill it now, so a test can watch a connection drop.
    ///
    /// The listener **and everything it forked**. OpenSSH runs each
    /// connection in an `sshd-session` child that puts itself in its own
    /// process group and its own session, so killing the listener - or its
    /// group - leaves the established connection up and this test would pass
    /// by not testing anything. The descendants are collected from `/proc`
    /// first and killed first, deepest last.
    fn kill(&mut self) {
        let mut doomed = descendants(self.child.id());
        doomed.push(self.child.id());
        if !doomed.is_empty() {
            let mut kill = Command::new("/usr/bin/kill");
            kill.arg("-KILL");
            for pid in &doomed {
                kill.arg(pid.to_string());
            }
            let _ = kill.stdout(Stdio::null()).stderr(Stdio::null()).status();
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Sshd {
    fn drop(&mut self) {
        self.kill();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Every process descended from `root`, deepest first, read from `/proc`.
///
/// `/proc/<pid>/status` rather than `stat`, because a process name can contain
/// a space and a bracket and `stat`'s second field then needs parsing rather
/// than splitting.
fn descendants(root: u32) -> Vec<u32> {
    let mut parents: Vec<(u32, u32)> = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|text| text.parse::<u32>().ok()) else {
            continue;
        };
        let Ok(status) = std::fs::read_to_string(entry.path().join("status")) else {
            continue;
        };
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("PPid:")
                && let Ok(ppid) = rest.trim().parse::<u32>()
            {
                parents.push((pid, ppid));
                break;
            }
        }
    }
    let mut found = vec![root];
    let mut at = 0usize;
    while let Some(&parent) = found.get(at) {
        for (pid, ppid) in &parents {
            if *ppid == parent && !found.contains(pid) {
                found.push(*pid);
            }
        }
        at = at.saturating_add(1);
    }
    // Without the root, and deepest first.
    found.remove(0);
    found.reverse();
    found
}

/// One throwaway ed25519 key pair, optionally encrypted.
fn keygen_with(path: &Path, passphrase: &str) -> Option<()> {
    let status = Command::new("ssh-keygen")
        .args([
            "-q",
            "-t",
            "ed25519",
            "-N",
            passphrase,
            "-C",
            "hcmd-test",
            "-f",
        ])
        .arg(path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()?;
    status.success().then_some(())
}

/// One throwaway ed25519 key pair with no passphrase.
fn keygen(path: &Path) -> Option<()> {
    keygen_with(path, "")
}

/// The saved host the plan is built from: key authentication and nothing else
/// reachable, so the test is not at the mercy of whatever agent happens to be
/// running.
fn host_for(server: &Sshd, user: &str) -> SavedHost {
    SavedHost {
        label: "loopback".to_string(),
        protocol: Protocol::Sftp,
        host: "127.0.0.1".to_string(),
        port: server.port,
        username: user.to_string(),
        auth: AuthMethod::Key,
        key_file: server.key.to_string_lossy().into_owned(),
        remote_dir: server.remote_dir(),
        local_dir: String::new(),
    }
}

/// Whoever is running the tests. A non-root `sshd` can log in nobody else.
fn current_user() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| "nobody".to_string())
}

/// Connect, answering the unknown-host prompt with `Accept`.
///
/// Returns the backend and how many times the host key was asked about, which
/// is what the second connect asserts is zero.
async fn connect(server: &Sshd) -> (Arc<SftpFs>, usize) {
    connect_as(server, "127.0.0.1").await
}

/// [`connect`] with the host written the way the quick-connect line would
/// write it, which for an IPv6 literal is bracketed.
async fn connect_as(server: &Sshd, host: &str) -> (Arc<SftpFs>, usize) {
    let user = current_user();
    let target = Target {
        protocol: Protocol::Sftp,
        host: host.to_string(),
        port: server.port,
        user: user.clone(),
        dir: Some(server.remote_dir()),
    };
    let plan = AuthPlan::for_host(&host_for(server, &user), &server.home);
    let (tx, mut rx) = mpsc::channel::<RemoteEvent>(8);
    let asked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = Arc::clone(&asked);
    let pump = tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            match event {
                RemoteEvent::HostKey {
                    fingerprint, reply, ..
                } => {
                    assert!(
                        fingerprint.starts_with("SHA256:"),
                        "the prompt shows what ssh-keygen -l shows: {fingerprint}"
                    );
                    counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let _ = reply.send(true);
                }
                RemoteEvent::HostKeyChanged { .. } => {
                    panic!("the key did not change");
                }
                RemoteEvent::Secret { reply, .. } => {
                    // Nothing here should ever need one: the plan is a key
                    // with no passphrase.
                    let _ = reply.send(None);
                }
                RemoteEvent::Connected { .. } | RemoteEvent::Failed { .. } => {}
            }
        }
    });
    let hooks = ConnectHooks::new(tx, ConnectId(1), server.known_hosts.clone());
    let config = RemoteConfig::default();
    let fs = SftpFs::connect(target, plan, None, &config, Arc::new(NoKeyring), hooks)
        .await
        .expect("connected");
    pump.abort();
    let count = asked.load(std::sync::atomic::Ordering::SeqCst);
    (fs, count)
}

/// Run one blocking transport call where it belongs.
///
async fn blocking<T, F>(work: F) -> T
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .expect("the blocking pool")
}

/// The layer the milestone is actually about: a **panel** reading a real
/// server through the ordinary `Vfs` trait.
///
/// > all of it goes through the same `Vfs` trait, so no operation needs
/// > to know whether it is local or remote
///
/// So this test uses no method of `SftpFs`: it registers the connection, takes
/// a `VfsPath` in the connection's own namespace, and drives `read_dir`,
/// `stat`, `open_write`, `open_read` and `remove` through `VfsRouter` exactly
/// as `main::spawn_read` and `ops::spawn` do. It is the one end-to-end check
/// that the integration - registry, ids, path arithmetic, the `..` row - is
/// right against something that is not a fake.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "starts a daemon and binds a loopback port; set HCMD_SSHD_TEST=1"]
async fn a_panel_reads_a_real_server_through_the_vfs_trait() {
    if !enabled() {
        return;
    }
    let Some(server) = Sshd::start("vfs") else {
        return;
    };
    let (transport, _asked) = connect(&server).await;
    let start = {
        let fs = Arc::clone(&transport);
        blocking(move || fs.start_dir()).await.expect("realpath")
    };

    let router = Arc::new(VfsRouter::new(
        ArchiveConfig::default(),
        RemoteConfig::default(),
    ));
    let backend = RemoteFs::new(
        Target {
            protocol: Protocol::Sftp,
            host: "127.0.0.1".to_string(),
            port: server.port,
            user: current_user(),
            dir: Some(start.clone()),
        },
        transport as Arc<dyn RemoteTransport>,
        Duration::from_secs(2),
    );
    let id = router.remotes().register(backend).expect("registered");
    let here = id.path(&start);

    // the header, from the path alone.
    assert_eq!(
        router.remotes().get(id).expect("open").header(&here),
        format!("sftp://{}@127.0.0.1:{}{start}", current_user(), server.port)
    );

    // Write a file through the trait, exactly as `ops::copy` does: `flush` is
    // the commit.
    let target = here.join("through-the-trait.txt");
    {
        let router = Arc::clone(&router);
        let target = target.clone();
        blocking(move || {
            let mut writer = Vfs::open_write(router.as_ref(), &target).expect("open_write");
            std::io::Write::write_all(&mut writer, b"panel bytes").expect("write");
            std::io::Write::flush(&mut writer).expect("flush is the commit");
        })
        .await;
    }

    // And read the directory back, through the same channel a panel reads.
    let mut rx = Vfs::read_dir(router.as_ref(), &here);
    let mut names = Vec::new();
    while let Some(row) = rx.recv().await {
        names.push(row.expect("a row").name);
    }
    assert_eq!(
        names.first().map(String::as_str),
        Some(".."),
        "the backend synthesises the parent row"
    );
    assert!(
        names.iter().any(|n| n == "through-the-trait.txt"),
        "the write is visible in the listing: {names:?}"
    );

    // `stat` and `open_read` agree with what was written.
    let entry = {
        let router = Arc::clone(&router);
        let target = target.clone();
        blocking(move || Vfs::stat(router.as_ref(), &target)).await
    }
    .expect("stat");
    assert_eq!(entry.size, 11);
    let body = {
        let router = Arc::clone(&router);
        let target = target.clone();
        blocking(move || {
            let mut reader = Vfs::open_read(router.as_ref(), &target).expect("open_read");
            let mut out = Vec::new();
            std::io::Read::read_to_end(&mut reader, &mut out).expect("read");
            out
        })
        .await
    };
    assert_eq!(body, b"panel bytes");

    // A path naming a connection nobody registered is refused rather than
    // serviced, which is what makes two tabs on two hosts two namespaces.
    let elsewhere = holoscommander::remote::RemoteId(999).path(&start);
    let refused = {
        let router = Arc::clone(&router);
        blocking(move || Vfs::stat(router.as_ref(), &elsewhere)).await
    };
    assert!(refused.is_err(), "a closed connection names nothing");

    {
        let router = Arc::clone(&router);
        let target = target.clone();
        blocking(move || Vfs::remove(router.as_ref(), &target)).await
    }
    .expect("remove");

    // Disconnecting closes it, and the path then names nothing rather than
    // someone else's host.
    router.remotes().close(id);
    assert!(router.remotes().get(id).is_none());
    let after = {
        let router = Arc::clone(&router);
        let here = here.clone();
        blocking(move || Vfs::stat(router.as_ref(), &here)).await
    };
    assert!(after.is_err(), "a stale path names nothing");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "starts a daemon and binds a loopback port; set HCMD_SSHD_TEST=1"]
async fn the_whole_transport_against_a_real_sftp_server() {
    if !enabled() {
        return;
    }
    let Some(server) = Sshd::start("all") else {
        return;
    };
    let root = server.remote_dir();

    // Connect. The host is unknown, so the fingerprint is shown and the
    // answer is Accept; the key is then in `known_hosts`.
    let (fs, asked) = connect(&server).await;
    assert_eq!(asked, 1, "an unknown host is asked about exactly once");
    assert!(
        std::fs::read_to_string(&server.known_hosts)
            .expect("known_hosts was written")
            .contains("ssh-ed25519"),
        "learn appended the accepted key"
    );

    // The login directory the server reports, which is what a target with no
    // directory of its own lands on.
    let start = {
        let fs = Arc::clone(&fs);
        blocking(move || fs.start_dir()).await.expect("realpath")
    };
    assert_eq!(start, root, "the target's directory, made absolute");

    // mkdir, upload, list, stat.
    let dir = format!("{root}/sub");
    {
        let fs = Arc::clone(&fs);
        let dir = dir.clone();
        blocking(move || fs.create_dir(&dir)).await.expect("mkdir");
    }
    let body: Vec<u8> = (0..300_000u32).map(|n| (n % 251) as u8).collect();
    let file = format!("{dir}/payload.bin");
    {
        let fs = Arc::clone(&fs);
        let file = file.clone();
        let body = body.clone();
        blocking(move || -> Result<(), Error> {
            let mut writer = fs.open_write(&file)?;
            // Chunked exactly as `ops::copy` does it, so the pipeline is the
            // one the copy engine will drive.
            for piece in body.chunks(64 * 1024) {
                writer.write_all(piece).map_err(Error::Bare)?;
            }
            // The commit.
            writer.flush().map_err(Error::Bare)
        })
        .await
        .expect("upload");
    }
    assert_eq!(
        std::fs::read(server.serving.join("sub").join("payload.bin")).expect("on disk"),
        body,
        "every byte arrived, in order"
    );

    let rows = {
        let fs = Arc::clone(&fs);
        let dir = dir.clone();
        blocking(move || fs.list(&dir)).await.expect("list")
    };
    assert_eq!(rows.len(), 1, "no `.` and no `..` from the transport");
    let row = rows.first().expect("the row");
    assert_eq!(row.name, "payload.bin");
    assert_eq!(row.size, body.len() as u64);
    assert_ne!(row.mode, 0, "SFTP reports mode bits and they are kept");

    let stat = {
        let fs = Arc::clone(&fs);
        let file = file.clone();
        blocking(move || fs.stat(&file)).await.expect("stat")
    };
    assert_eq!(stat.size, body.len() as u64);

    // Download, through the pipelined reader.
    let got = {
        let fs = Arc::clone(&fs);
        let file = file.clone();
        blocking(move || -> Result<Vec<u8>, Error> {
            let mut reader = fs.open_read(&file)?;
            let mut out = Vec::new();
            reader.read_to_end(&mut out).map_err(Error::Bare)?;
            Ok(out)
        })
        .await
        .expect("download")
    };
    assert_eq!(got, body, "the pipeline delivered the file byte for byte");

    // A seeking read, which is what the viewer does.
    let window = {
        let fs = Arc::clone(&fs);
        let file = file.clone();
        blocking(move || -> Result<Vec<u8>, Error> {
            let mut reader = fs.open_seek(&file)?;
            reader.seek(SeekFrom::Start(200_000)).map_err(Error::Bare)?;
            let mut out = vec![0u8; 16];
            reader.read_exact(&mut out).map_err(Error::Bare)?;
            Ok(out)
        })
        .await
        .expect("seek")
    };
    assert_eq!(
        window,
        body.get(200_000..200_016).expect("the slice").to_vec(),
        "a window is fetched without reading what is before it"
    );

    // Rename, then delete, then the directory.
    let renamed = format!("{dir}/renamed.bin");
    {
        let fs = Arc::clone(&fs);
        let (from, to) = (file.clone(), renamed.clone());
        blocking(move || fs.rename(&from, &to))
            .await
            .expect("rename");
    }
    assert!(server.serving.join("sub").join("renamed.bin").exists());
    {
        let fs = Arc::clone(&fs);
        let renamed = renamed.clone();
        blocking(move || fs.remove_file(&renamed))
            .await
            .expect("remove");
    }
    {
        let fs = Arc::clone(&fs);
        let dir = dir.clone();
        blocking(move || fs.remove_dir(&dir)).await.expect("rmdir");
    }
    assert!(!server.serving.join("sub").exists());

    // Permissions.
    let marked = format!("{root}/mode.txt");
    std::fs::write(server.serving.join("mode.txt"), b"x").expect("a file to chmod");
    {
        let fs = Arc::clone(&fs);
        let marked = marked.clone();
        blocking(move || fs.set_permissions(&marked, 0o600))
            .await
            .expect("setstat");
    }
    let after = {
        let fs = Arc::clone(&fs);
        let marked = marked.clone();
        blocking(move || fs.stat(&marked)).await.expect("stat")
    };
    assert_eq!(after.mode & 0o777, 0o600);

    // A missing file is `NotFound`, not a generic failure.
    let missing = {
        let fs = Arc::clone(&fs);
        let gone = format!("{root}/gone");
        blocking(move || fs.stat(&gone)).await
    };
    assert!(matches!(missing, Err(Error::NotFound(_))), "{missing:?}");

    assert!(fs.is_live());
}

/// the design accepts a bracketed IPv6 literal on the quick-connect line -
/// `sftp://[::1]:2222` - and it is the only spelling that can carry a port.
/// the design verifies the host key under the name `ssh` writes.
///
/// Both halves used to be broken and neither could be seen without a server:
/// `TcpStream::connect(("[::1]", port))` fails with "Name or service not
/// known" whatever is listening, and `known_hosts`'s lookup name would have
/// been the doubly bracketed `[[::1]]:port`, matching no entry a user has.
/// FTP already trimmed the brackets at its own socket; SFTP now asks
/// `Target::hostname` the same question.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "starts a daemon and binds a loopback port; set HCMD_SSHD_TEST=1"]
async fn a_bracketed_ipv6_host_connects_and_is_recorded_the_way_ssh_writes_it() {
    if !enabled() {
        return;
    }
    // `None` on a machine with no IPv6 loopback, which is a skip.
    let Some(server) = Sshd::start_on("ipv6", "::1") else {
        return;
    };
    let (fs, asked) = connect_as(&server, "[::1]").await;
    assert_eq!(asked, 1, "a first connection asks about the host key");
    assert!(fs.is_live(), "the bracketed literal reached the socket");

    let recorded = std::fs::read_to_string(&server.known_hosts).expect("known_hosts");
    let expected = format!("[::1]:{}", server.port);
    assert!(
        recorded.starts_with(&expected),
        "one pair of brackets, as ssh writes them: {recorded:?}"
    );
    assert!(
        !recorded.contains("[["),
        "never the doubly bracketed form: {recorded:?}"
    );

    // And the entry that was just written is found again, which is the half a
    // user with an existing `known_hosts` would have lost.
    drop(fs);
    let (again, asked) = connect_as(&server, "[::1]").await;
    assert_eq!(asked, 0, "a known host is not asked about again");
    assert!(again.is_live());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "starts a daemon and binds a loopback port; set HCMD_SSHD_TEST=1"]
async fn a_second_connect_finds_the_key_already_known() {
    if !enabled() {
        return;
    }
    let Some(server) = Sshd::start("known") else {
        return;
    };
    let (first, asked) = connect(&server).await;
    assert_eq!(asked, 1);
    drop(first);
    // The same file, the same key: the `Known`, and no question.
    let (second, asked) = connect(&server).await;
    assert_eq!(asked, 0, "a known host is not asked about again");
    assert!(second.is_live());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "starts a daemon and binds a loopback port; set HCMD_SSHD_TEST=1"]
async fn a_dropped_connection_is_reported_and_not_a_hang() {
    if !enabled() {
        return;
    }
    let Some(mut server) = Sshd::start("drop") else {
        return;
    };
    let root = server.remote_dir();
    let (fs, _) = connect(&server).await;
    {
        let fs = Arc::clone(&fs);
        let root = root.clone();
        blocking(move || fs.list(&root))
            .await
            .expect("a listing first");
    }
    server.kill();
    // The daemon is gone. Every subsequent call must answer - with
    // `ConnectionLost`, which is what stops a batch rather than failing two
    // hundred files identically.
    let outcome = {
        let fs = Arc::clone(&fs);
        let root = root.clone();
        blocking(move || fs.list(&root)).await
    };
    assert!(outcome.is_err(), "a dead server does not answer a listing");
    // ...and the panel's every-frame question answers false without I/O.
    let deadline = Instant::now() + Duration::from_secs(5);
    while fs.is_live() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(!fs.is_live(), "the disconnected state is reached");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "starts a daemon and binds a loopback port; set HCMD_SSHD_TEST=1"]
async fn a_changed_host_key_is_refused_and_offers_nothing_to_click() {
    if !enabled() {
        return;
    }
    let Some(mut server) = Sshd::start("changed") else {
        return;
    };
    let (first, asked) = connect(&server).await;
    assert_eq!(asked, 1);
    drop(first);
    let recorded = std::fs::read(&server.known_hosts).expect("known_hosts");

    if !server.restart_with_a_new_host_key() {
        return;
    }

    // The same host and port, a different key. refuse loudly and
    // offer no one-key override - which is enforced by
    // `RemoteEvent::HostKeyChanged` having no reply channel at all (S6).
    let user = current_user();
    let target = Target {
        protocol: Protocol::Sftp,
        host: "127.0.0.1".to_string(),
        port: server.port,
        user: user.clone(),
        dir: Some(server.remote_dir()),
    };
    let plan = AuthPlan::for_host(&host_for(&server, &user), &server.home);
    let (tx, mut rx) = mpsc::channel::<RemoteEvent>(8);
    let hooks = ConnectHooks::new(tx, ConnectId(2), server.known_hosts.clone());
    let config = RemoteConfig::default();
    let attempt = tokio::spawn(async move {
        SftpFs::connect(target, plan, None, &config, Arc::new(NoKeyring), hooks).await
    });

    let event = tokio::time::timeout(Duration::from_secs(15), rx.recv())
        .await
        .expect("an event arrives")
        .expect("the channel is open");
    match event {
        RemoteEvent::HostKeyChanged {
            target, line, file, ..
        } => {
            assert_eq!(target.host, "127.0.0.1");
            assert!(line >= 1, "the message names the line to go and look at");
            assert_eq!(file, server.known_hosts);
        }
        RemoteEvent::HostKey { .. } => panic!("a changed key must not be offered as unknown"),
        RemoteEvent::Secret { .. } | RemoteEvent::Connected { .. } | RemoteEvent::Failed { .. } => {
            panic!("the wrong question was asked")
        }
    }

    let outcome = tokio::time::timeout(Duration::from_secs(15), attempt)
        .await
        .expect("the attempt finishes")
        .expect("the task");
    assert!(outcome.is_err(), "a changed host key is never connected to");
    assert_eq!(
        std::fs::read(&server.known_hosts).expect("known_hosts"),
        recorded,
        "nothing was learned, appended or rewritten (S6)"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "starts a daemon and binds a loopback port; set HCMD_SSHD_TEST=1"]
async fn declining_an_unknown_host_key_connects_to_nothing_and_learns_nothing() {
    if !enabled() {
        return;
    }
    let Some(server) = Sshd::start("declined") else {
        return;
    };
    let user = current_user();
    let target = Target {
        protocol: Protocol::Sftp,
        host: "127.0.0.1".to_string(),
        port: server.port,
        user: user.clone(),
        dir: Some(server.remote_dir()),
    };
    let plan = AuthPlan::for_host(&host_for(&server, &user), &server.home);
    let (tx, mut rx) = mpsc::channel::<RemoteEvent>(8);
    let hooks = ConnectHooks::new(tx, ConnectId(3), server.known_hosts.clone());
    let config = RemoteConfig::default();
    let attempt = tokio::spawn(async move {
        SftpFs::connect(target, plan, None, &config, Arc::new(NoKeyring), hooks).await
    });

    // Cancel, by dropping the reply channel rather than by answering `false`:
    // that is the path `Esc` takes, and it must refuse exactly as a `Cancel`
    // does.
    let event = tokio::time::timeout(Duration::from_secs(15), rx.recv())
        .await
        .expect("an event arrives")
        .expect("the channel is open");
    match event {
        RemoteEvent::HostKey { reply, .. } => drop(reply),
        RemoteEvent::HostKeyChanged { .. }
        | RemoteEvent::Secret { .. }
        | RemoteEvent::Connected { .. }
        | RemoteEvent::Failed { .. } => panic!("an unknown host is asked about, once"),
    }

    let outcome = tokio::time::timeout(Duration::from_secs(15), attempt)
        .await
        .expect("the attempt finishes")
        .expect("the task");
    assert!(outcome.is_err(), "never a default of accepting");
    assert!(
        !server.known_hosts.exists(),
        "a refused key is not written to known_hosts"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "starts a daemon and binds a loopback port; set HCMD_SSHD_TEST=1"]
async fn an_encrypted_key_is_asked_about_and_a_wrong_passphrase_is_asked_about_again() {
    if !enabled() {
        return;
    }
    let Some(server) = Sshd::start("passphrase") else {
        return;
    };
    // Replace the user key with an encrypted one, and re-authorise it.
    let key = server.dir.join("locked_ed25519");
    if keygen_with(&key, "correct horse").is_none() {
        return;
    }
    std::fs::copy(
        key.with_extension("pub"),
        server.dir.join("authorized_keys"),
    )
    .expect("authorized_keys");

    let user = current_user();
    let target = Target {
        protocol: Protocol::Sftp,
        host: "127.0.0.1".to_string(),
        port: server.port,
        user: user.clone(),
        dir: Some(server.remote_dir()),
    };
    let host = SavedHost {
        key_file: key.to_string_lossy().into_owned(),
        ..host_for(&server, &user)
    };
    let plan = AuthPlan::for_host(&host, &server.home);
    let (tx, mut rx) = mpsc::channel::<RemoteEvent>(8);
    let asked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = Arc::clone(&asked);
    let pump = tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            match event {
                RemoteEvent::HostKey { reply, .. } => {
                    let _ = reply.send(true);
                }
                RemoteEvent::Secret {
                    kind,
                    offer_keyring,
                    reply,
                    ..
                } => {
                    assert!(
                        matches!(kind, SecretKind::Passphrase { .. }),
                        "an encrypted key asks for a passphrase, not a password"
                    );
                    assert!(
                        !offer_keyring,
                        "a key passphrase is not what the keyring opt-in is about"
                    );
                    let n = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    // Wrong the first time, right the second: `Outcome::Needs`
                    // must not advance past the method.
                    //
                    let word = if n == 0 { "wrong" } else { "correct horse" };
                    let _ = reply.send(Some(SecretAnswer {
                        secret: Secret::from_str(word),
                        remember: false,
                    }));
                }
                RemoteEvent::HostKeyChanged { .. } => panic!("the key did not change"),
                RemoteEvent::Connected { .. } | RemoteEvent::Failed { .. } => {}
            }
        }
    });
    let hooks = ConnectHooks::new(tx, ConnectId(4), server.known_hosts.clone());
    let config = RemoteConfig::default();
    let fs = SftpFs::connect(target, plan, None, &config, Arc::new(NoKeyring), hooks)
        .await
        .expect("the second passphrase opens the key");
    pump.abort();
    assert_eq!(
        asked.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "asked twice and no more"
    );
    assert!(fs.is_live());
}

/// Reported from a real session: entering a zip on a remote answered "that
/// connection has been closed", and the panel could not get back to local.
///
/// the design over: the container is fetched into the session cache and opened
/// locally, because every archive format seeks and `Capabilities::SFTP` says
/// random access is not cheap. This drives the real path end to end.
#[tokio::test]
#[ignore = "needs a real sshd; set HCMD_SSHD_TEST=1"]
async fn a_zip_on_a_remote_opens_through_the_session_cache() {
    if !enabled() {
        return;
    }
    let Some(server) = Sshd::start("zip") else {
        return;
    };
    let (transport, _asked) = connect(&server).await;
    let start = {
        let fs = Arc::clone(&transport);
        blocking(move || fs.start_dir()).await.expect("realpath")
    };

    let router = Arc::new(VfsRouter::new(
        ArchiveConfig::default(),
        RemoteConfig::default(),
    ));
    let backend = RemoteFs::new(
        Target {
            protocol: Protocol::Sftp,
            host: "127.0.0.1".to_string(),
            port: server.port,
            user: current_user(),
            dir: Some(start.clone()),
        },
        transport as Arc<dyn RemoteTransport>,
        Duration::from_secs(5),
    );
    let id = router.remotes().register(backend).expect("registered");
    let here = id.path(&start);

    // A real zip, written onto the server through the trait.
    let zip = {
        let mut buf = Vec::new();
        {
            let mut w = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            w.start_file(
                "inside.txt",
                zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Stored),
            )
            .expect("start_file");
            std::io::Write::write_all(&mut w, b"hello from inside").expect("write");
            w.finish().expect("finish");
        }
        buf
    };
    let remote_zip = here.join("bundle.zip");
    {
        let router = Arc::clone(&router);
        let remote_zip = remote_zip.clone();
        let zip = zip.clone();
        blocking(move || {
            let mut writer = Vfs::open_write(router.as_ref(), &remote_zip).expect("open_write");
            std::io::Write::write_all(&mut writer, &zip).expect("write");
            std::io::Write::flush(&mut writer).expect("flush");
        })
        .await;
    }

    // Enter it, exactly as `Enter` on the panel does.
    let inside = remote_zip
        .clone()
        .with_segment(holoscommander::vfs::BackendKind::Archive, "/");
    let mut rx = Vfs::read_dir(router.as_ref(), &inside);
    let mut names = Vec::new();
    while let Some(row) = rx.recv().await {
        match row {
            Ok(entry) => names.push(entry.name),
            Err(err) => panic!("reading a remote zip failed: {err}"),
        }
    }
    assert!(
        names.iter().any(|n| n == "inside.txt"),
        "the member is listed: {names:?}"
    );
}

/// Reported from a real session: `Alt+F7` over a connected panel counted its
/// hits in the status line and listed none of them.
///
/// the design puts the results in the panel that was active when the search
/// started and streams them in as they are found, and the mechanism is the
/// ordinary directory-read channel: the walk fills a `ListFs`, and the panel
/// reads it back through `Vfs::read_dir` and `app::stream_read`. The status
/// line reads the listing directly, so it was right while the panel was empty.
///
/// The tree is deliberately lopsided - the hits are in the first directory the
/// walk reads and the bulk of the walk is the thousands of directories after
/// them - because that is the shape the bug needed: a handful of hits found
/// early, and a walk that goes on long enough for "not until it ends" to be
/// visible to a person. It is asserted as "the panel had rows while the
/// listing was still filling", never as a duration.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "starts a daemon and binds a loopback port; set HCMD_SSHD_TEST=1"]
async fn a_search_over_a_connected_panel_fills_it_while_the_walk_runs() {
    if !enabled() {
        return;
    }
    let Some(server) = Sshd::start("search") else {
        return;
    };
    let (transport, _asked) = connect(&server).await;
    let start = {
        let fs = Arc::clone(&transport);
        blocking(move || fs.start_dir()).await.expect("realpath")
    };

    // Two hits and a miss, all in the directory the walk reads first.
    std::fs::write(server.serving.join("found.txt"), b"top\n").expect("a hit");
    std::fs::create_dir_all(server.serving.join("sub")).expect("a subdirectory");
    std::fs::write(server.serving.join("sub").join("deep.txt"), b"deeper\n").expect("a hit");
    std::fs::write(server.serving.join("skip.log"), b"not this one\n").expect("a miss");
    // And then a great many directories with nothing in them, which is what
    // the rest of the walk is spent on. Fewer than 128 hits, deliberately:
    // a full batch was always sent, and the rows this test is about are the
    // ones in the batch that never filled.
    let bulk = server.serving.join("bulk");
    for n in 0..4_000 {
        std::fs::create_dir_all(bulk.join(format!("d{n:04}"))).expect("a directory to walk");
    }

    let mut app = App::headless(Config::default(), Keymap::builtin(), Theme::blue());
    let target = Target {
        protocol: Protocol::Sftp,
        host: "127.0.0.1".to_string(),
        port: server.port,
        user: current_user(),
        dir: Some(start.clone()),
    };
    let backend = RemoteFs::new(
        target.clone(),
        transport as Arc<dyn RemoteTransport>,
        Duration::from_secs(5),
    );
    let id = app.remotes().register(backend).expect("registered");

    // Connect the active panel the way `Ctrl+F` does: the dialog's answer is a
    // queued request, and the event the connect task sends back puts the tab
    // on the connection.
    app.connect_answered(Box::new(ConnectAnswer {
        target,
        plan: AuthPlan::for_password_login(None),
        password: None,
        local_dir: None,
        hosts: None,
    }));
    let request = app.take_pending_connect().expect("the connect was queued");
    app.apply_remote_event(RemoteEvent::Connected {
        attempt: request.attempt,
        id,
        start: id.path(&start),
        saved: None,
    });
    let _ = app.take_pending_reads();
    assert!(app.left.active_tab().is_remote(), "the panel is connected");

    // `Alt+F7` with a mask and nothing else: a name-only search of a remote
    // root needs no opt-in, because it reads listings and that is what
    // browsing already does.
    let mut query = Query::new(id.path(&start));
    query.name = "*.txt".to_string();
    app.request_search(query, VirtualKind::Search);
    let request = app.take_pending_search().expect("the search was queued");
    let started = app.start_search(*request).expect("the search started");

    // Everything from here is the ordinary directory-read path, driven the way
    // the event loop drives it: the same `stream_read`, the same channel, the
    // same `apply_vfs_event`.
    let reads = app.take_pending_reads();
    assert_eq!(reads.len(), 1, "the listing is read: {reads:?}");
    let read = reads.into_iter().next().expect("one read");
    let (tx, mut rx) = mpsc::channel::<VfsEvent>(256);
    tokio::spawn(stream_read(Arc::clone(&app.vfs), read, tx));

    let listing = app
        .listing(Side::Left, 0)
        .expect("the panel is showing one");
    let mut filled_while_walking = false;
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        assert!(Instant::now() < deadline, "the search never finished");
        match tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
            Ok(Some(event)) => app.apply_vfs_event(event),
            Ok(None) => break,
            Err(_) => continue,
        }
        // The status is read *after* the rows are in the panel, so a walk that
        // ended in between reads as ended: the claim only ever gets weaker
        // this way, and it is still "these rows were on screen before the walk
        // was over".
        if !app.left.active_tab().entries.is_empty() && !listing.status().is_final() {
            filled_while_walking = true;
        }
    }
    let tally = started.walk.await.expect("the walk finished");

    assert!(
        filled_while_walking,
        "the hits reach the panel as they are found, not when \
         the walk ends"
    );
    assert_eq!(tally.matched, 2, "the walk found both hits");
    assert_eq!(listing.len(), 2, "and pushed both into the listing");
    let names: Vec<String> = app
        .left
        .active_tab()
        .entries
        .iter()
        .map(|entry| entry.name.clone())
        .collect();
    assert_eq!(
        names,
        vec!["deep.txt".to_string(), "found.txt".to_string()],
        "and the panel is holding them"
    );

    // Every row addresses the real file on the server, so F3, F5 and F8 reach
    // it.
    for entry in &app.left.active_tab().entries {
        let location = entry.location.as_ref().expect("a real address");
        assert_eq!(
            holoscommander::remote::RemoteId::from_path(location),
            Some(id),
            "{location} is on the connection the search ran over"
        );
    }
}
