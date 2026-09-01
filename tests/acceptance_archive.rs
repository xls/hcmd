//! The twelve v0.5 acceptance criteria, driven through the real
//! `hcmd` binary, plus the terminal contract with an archive open.
//!
//! Same harness as `tests/acceptance.rs` and `tests/acceptance_ops.rs` - a pty,
//! real key bytes, a `vt100` screen - with the rule `tests/acceptance_ops.rs`
//! introduced and this milestone needs twice over:
//!
//! > **every criterion that produces bytes asserts on the bytes.**
//!
//! A panel that says `copied 1 file` and a `.zip` that really holds a new
//! member are two different claims, and only the second is the feature. So the
//! archives are opened again afterwards - with `zip`, `tar` and `flate2`, in
//! this file - and read.
//!
//! # Every fixture is built here, by the crates themselves
//!
//! There is not one archive checked into the repository. A `.zip` is written
//! with `zip`, the compressed tars with `tar` + `flate2`, and nothing shells
//! out to `zip(1)` or `tar(1)` - the design settles that for the product, and
//! a test that reached for a subprocess to build what the product must build
//! in-process would be testing a different program.
//!
//! **The `.rar` is the exception that proves it.** Nothing in the design's
//! crate table can *write* a RAR - that is what "read only" in the design's
//! table means - and the design forbids shelling out to `rar` to make one. So
//! [`write_rar`] assembles a RAR 4.x container out of the format itself: a
//! marker, a main header, stored (uncompressed) file headers and a terminator.
//! That is not a RAR *writer* and is not shipped; it is a fixture, it lives in
//! this file, and it is the same one `src/vfs/archive/rar.rs`'s own tests use.
//! Criterion 6 needs a real archive that the real `unrar` can list, and this is
//! the only way to have one without committing a binary blob.
//!
//! # Three things this harness needs that the earlier ones do not
//!
//! * **The archive temp directory is pinned** with `[archive] temp_dir`, so a
//!   test can look at the session cache rather than guess at it, and so a run
//!   cannot touch the developer's own `$TMPDIR`.
//! * **[`Session::wait_now`] does not wait for the screen to settle.** A panel
//!   filling from an archive index repaints continuously, so "unchanged for a
//!   beat" never comes true while it does - which is precisely the moment
//!   criterion 2 is about.
//! * **The cursor is followed by colour, not by text.** Which row the cursor is
//!   on is a *style*, invisible in the screen's plain text, so
//!   [`Session::cursor_row`] reads the painted cells. It works on whichever
//!   panel has focus, because half of these criteria need the other panel
//!   driven into an archive first.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "an integration test is its own crate, so it is not #[cfg(test)] \
              and clippy.toml's allow-*-in-tests keys do not reach it. \
              Panicking assertions are the point of a test."
)]

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

/// Keeps concurrently running tests off each other's temporary directories.
static NEXT: AtomicUsize = AtomicUsize::new(0);

/// Key encodings, verified against crossterm 0.29's
/// `src/event/sys/unix/parse.rs`.
///
/// The `CSI <n> ~` family is `parse_csi_special_key_code`: `11..=15` are `F1`
/// to `F5` and `17..=21` are `F6` to `F10`, so `F3` is `13`, `F5` is `15` and
/// `F6` is `17`. A modifier goes in a second field as a bitmask offset by one,
/// so `;3` is Alt - which is what makes `Alt+F5` `ESC [ 15 ; 3 ~` and
/// `Alt+F6` `ESC [ 17 ; 3 ~`, the two keys.
///
/// The `CSI <codepoint> ; <modifier> u` family is the Kitty protocol's, and the
/// codepoint is the key's *unshifted* Unicode value: `Ctrl+G` is `103` (`'g'`).
mod keys {
    pub const UP: &[u8] = b"\x1b[A";
    pub const DOWN: &[u8] = b"\x1b[B";
    pub const ENTER: &[u8] = b"\r";
    pub const ESC: &[u8] = b"\x1b";
    pub const TAB: &[u8] = b"\t";
    pub const BACKSPACE: &[u8] = b"\x7f";
    pub const INSERT: &[u8] = b"\x1b[2~";

    pub const F3: &[u8] = b"\x1b[13~";
    pub const F5: &[u8] = b"\x1b[15~";
    pub const F10: &[u8] = b"\x1b[21~";
    /// `Alt+F5` - pack the selection.
    pub const ALT_F5: &[u8] = b"\x1b[15;3~";
    /// `Alt+F6` - unpack the archive under the cursor.
    pub const ALT_F6: &[u8] = b"\x1b[17;3~";

    /// `Ctrl+G` - go to a path, which is how a test points a
    /// panel somewhere without a command line.
    pub const CTRL_G: &[u8] = b"\x1b[103;5u";
    /// `Ctrl+R` - reread the panel.
    pub const CTRL_R: &[u8] = b"\x1b[114;5u";
}

/// The theme colours the assertions read back out of the rendered cells.
///
/// These are the compiled-in defaults, which `themes/blue.toml`
/// mirrors, and `COLORTERM=truecolor` keeps them unquantised.
mod paint {
    /// `panel.cursor_bg` - the focused panel's cursor bar.
    pub const CURSOR_FOCUSED: (u8, u8, u8) = (0x00, 0xA8, 0xA8);
}

/// The bytes `Term::restore` writes, in the order it writes them.
mod restore {
    pub const ALT_ON: &str = "\x1b[?1049h";
    pub const ALT_OFF: &str = "\x1b[?1049l";
    /// `Show`, emitted *after* `disable_raw_mode` and followed by nothing.
    pub const CURSOR_SHOWN: &str = "\x1b[?25h";
}

// ---------------------------------------------------------------------------
// The fixture tree
// ---------------------------------------------------------------------------

/// A `<root>/src`, `<root>/dst` and `<root>/temp` triple, built per test and
/// removed on drop.
///
/// `temp` is the pinned `[archive] temp_dir`: the session cache goes there
/// rather than into the developer's `$TMPDIR`.
struct Tree {
    root: PathBuf,
}

impl Tree {
    fn new(tag: &str) -> Self {
        // Deliberately short. The panel's top border carries the path
        // and crops it in the middle, and criterion 1 reads
        // `foo.zip#/` back off it.
        let root = std::env::temp_dir().join(format!(
            "hcmd-ar-{}-{}-{tag}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).expect("fixture src");
        fs::create_dir_all(root.join("dst")).expect("fixture dst");
        fs::create_dir_all(root.join("temp")).expect("archive temp dir");
        Self { root }
    }

    fn root(&self) -> &Path {
        &self.root
    }

    fn src(&self) -> PathBuf {
        self.root.join("src")
    }

    fn dst(&self) -> PathBuf {
        self.root.join("dst")
    }

    fn temp(&self) -> PathBuf {
        self.root.join("temp")
    }

    /// Write a file under the root, creating its parents.
    fn file(&self, rel: &str, bytes: &[u8]) -> PathBuf {
        let path = self.root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("fixture parent");
        }
        fs::write(&path, bytes).expect("fixture file");
        path
    }

    /// Make a directory under the root.
    fn dir(&self, rel: &str) -> PathBuf {
        let path = self.root.join(rel);
        fs::create_dir_all(&path).expect("fixture dir");
        path
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

// ---------------------------------------------------------------------------
// Building the archives, with the crates themselves
// ---------------------------------------------------------------------------

/// A `.zip` holding `members`, deflated.
fn write_zip(path: &Path, members: &[(&str, &[u8])]) {
    let file = fs::File::create(path).expect("create the zip");
    let mut writer = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o644);
    for (name, body) in members {
        // `start_file` stores the name exactly as given - it does not
        // normalise, and it does not refuse `..`. That is what lets criterion 9
        // build a real Zip Slip archive rather than a pretend one.
        writer.start_file(*name, options).expect("start the member");
        writer.write_all(body).expect("write the member");
    }
    writer.finish().expect("finish the zip");
}

/// The member names in a `.zip`, in the container's own order.
fn zip_names(path: &Path) -> Vec<String> {
    let file = fs::File::open(path).expect("open the zip");
    let mut archive = zip::ZipArchive::new(file).expect("read the zip");
    (0..archive.len())
        .map(|i| {
            archive
                .by_index(i)
                .expect("a member")
                .name()
                .trim_end_matches('/')
                .to_string()
        })
        .collect()
}

/// One member's bytes out of a `.zip`.
fn zip_member(path: &Path, name: &str) -> Vec<u8> {
    let file = fs::File::open(path).expect("open the zip");
    let mut archive = zip::ZipArchive::new(file).expect("read the zip");
    let mut member = archive
        .by_name(name)
        .unwrap_or_else(|e| panic!("{name} in {}: {e}", path.display()));
    let mut out = Vec::new();
    member.read_to_end(&mut out).expect("read the member");
    out
}

/// One tar member as this file describes it: a name, a kind, and bytes.
enum TarMember<'a> {
    File(&'a str, &'a [u8]),
    /// A symbolic link and what it points at - criterion 10's whole subject.
    Link(&'a str, &'a str),
}

/// A `.tar.gz` holding `members`.
fn write_tar_gz(path: &Path, members: &[TarMember<'_>]) {
    let file = fs::File::create(path).expect("create the tar.gz");
    let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
    let mut builder = tar::Builder::new(encoder);
    for member in members {
        let mut header = tar::Header::new_gnu();
        header.set_mode(0o644);
        header.set_mtime(1_700_000_000);
        match member {
            TarMember::File(name, body) => {
                header.set_entry_type(tar::EntryType::Regular);
                header.set_size(body.len() as u64);
                header.set_cksum();
                builder
                    .append_data(&mut header, name, std::io::Cursor::new(*body))
                    .expect("append the member");
            }
            TarMember::Link(name, target) => {
                header.set_entry_type(tar::EntryType::Symlink);
                header.set_size(0);
                builder
                    .append_link(&mut header, name, target)
                    .expect("append the link");
            }
        }
    }
    builder
        .into_inner()
        .expect("finish the tar")
        .finish()
        .expect("finish the gzip");
}

/// Every regular member of a `.tar.gz`, as `(name, bytes)`.
///
/// This is the "and readable" half of criterion 11: a container that cannot be
/// walked from its first header to its last is not intact, whatever its size
/// says.
fn tar_gz_members(path: &Path) -> Vec<(String, Vec<u8>)> {
    let file = fs::File::open(path).expect("open the tar.gz");
    let decoder = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);
    let mut out = Vec::new();
    for entry in archive.entries().expect("entries") {
        let mut entry = entry.expect("a member header");
        let name = entry
            .path()
            .expect("a member path")
            .to_string_lossy()
            .into_owned();
        let mut body = Vec::new();
        entry.read_to_end(&mut body).expect("member bytes");
        out.push((name, body));
    }
    out
}

/// CRC-32, which a RAR 4.x block header carries the low sixteen bits of.
///
/// Written out rather than pulled in: `crc32fast` is not one of the design's
/// crates, and this is eleven lines used by [`write_rar`] and by nothing that
/// ships.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffff_u32;
    for byte in data {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = 0_u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}

/// One RAR 4.x block: `HEAD_CRC`, `HEAD_TYPE`, `HEAD_FLAGS`, `HEAD_SIZE`, the
/// type's own fields, and whatever data follows the header.
fn rar_block(kind: u8, flags: u16, body: &[u8], data: &[u8]) -> Vec<u8> {
    let size = u16::try_from(7_usize.saturating_add(body.len())).expect("a short header");
    let mut head = vec![kind];
    head.extend_from_slice(&flags.to_le_bytes());
    head.extend_from_slice(&size.to_le_bytes());
    head.extend_from_slice(body);
    let mut out = u16::try_from(crc32(&head) & 0xffff)
        .expect("sixteen bits")
        .to_le_bytes()
        .to_vec();
    out.extend_from_slice(&head);
    out.extend_from_slice(data);
    out
}

/// A RAR 4.x file header (type `0x74`) with the member stored, not packed.
fn rar_file(name: &str, data: &[u8]) -> Vec<u8> {
    // 2024-02-29 12:34:56 as a DOS timestamp.
    const FTIME: u32 = (44 << 25) | (2 << 21) | (29 << 16) | (12 << 11) | (34 << 5) | 28;
    let name = name.as_bytes();
    let mut body = Vec::new();
    let len = u32::try_from(data.len()).expect("a small fixture");
    body.extend_from_slice(&len.to_le_bytes()); // PACK_SIZE
    body.extend_from_slice(&len.to_le_bytes()); // UNP_SIZE
    body.push(3); // HOST_OS: Unix
    body.extend_from_slice(&crc32(data).to_le_bytes());
    body.extend_from_slice(&FTIME.to_le_bytes());
    body.push(20); // UNP_VER
    body.push(0x30); // METHOD: stored
    body.extend_from_slice(
        &u16::try_from(name.len())
            .expect("a short name")
            .to_le_bytes(),
    );
    body.extend_from_slice(&0o644_u32.to_le_bytes()); // ATTR
    body.extend_from_slice(name);
    // `LHD_LONG_BLOCK`.
    rar_block(0x74, 0x8000, &body, data)
}

/// A whole RAR 4.x container: marker, main header, the members, terminator.
///
/// See this module's own documentation for why a fixture is assembled by hand
/// here and nowhere else.
fn write_rar(path: &Path, members: &[(&str, &[u8])]) {
    let mut out = b"Rar!\x1a\x07\x00".to_vec();
    let mut main = Vec::new();
    main.extend_from_slice(&0_u16.to_le_bytes()); // HighPosAv
    main.extend_from_slice(&0_u32.to_le_bytes()); // PosAv
    out.extend_from_slice(&rar_block(0x73, 0x0000, &main, &[]));
    for (name, data) in members {
        out.extend_from_slice(&rar_file(name, data));
    }
    out.extend_from_slice(&rar_block(0x7b, 0x4000, &[], &[]));
    fs::write(path, out).expect("write the rar fixture");
}

/// A `.tar.gz` big enough that indexing it is still running when the test
/// presses the next key: 900 members of 1 MiB, which is most of a gigabyte of
/// tar to walk and a few megabytes on disk.
fn write_streaming_tar_gz(path: &Path) {
    let file = fs::File::create(path).expect("create");
    let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
    let mut builder = tar::Builder::new(encoder);
    let mut body = Vec::with_capacity(1024 * 1024);
    let mut n = 0_u64;
    while body.len() < 1024 * 1024 {
        let _ = writeln!(body, "line {n} of a member that exists to take up room");
        n = n.wrapping_add(1);
    }
    body.truncate(1024 * 1024);
    for i in 0..900 {
        let mut header = tar::Header::new_gnu();
        header.set_size(body.len() as u64);
        header.set_mode(0o644);
        header.set_mtime(1_700_000_000);
        header.set_cksum();
        builder
            .append_data(
                &mut header,
                format!("member-{i:04}.txt"),
                std::io::Cursor::new(&body),
            )
            .expect("append");
    }
    builder
        .into_inner()
        .expect("finish tar")
        .finish()
        .expect("finish gzip");
}

/// Bytes that do not compress to nothing and are not all the same, so "byte
/// for byte" is a real claim about a real payload.
fn payload(len: usize, seed: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    let mut x = seed | 1;
    while out.len() < len {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        out.extend_from_slice(&x.to_le_bytes());
    }
    out.truncate(len);
    out
}

// ---------------------------------------------------------------------------
// Filesystem assertions
// ---------------------------------------------------------------------------

fn read(path: &Path) -> Vec<u8> {
    fs::read(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// The names directly inside a directory, sorted. `[]` for one that is not
/// there at all, which is what "nothing was created" looks like.
fn listing(dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Every path under `dir`, recursively, relative to it, sorted.
fn walk(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        let Ok(entries) = fs::read_dir(&next) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if let Ok(rel) = path.strip_prefix(dir) {
                out.push(rel.to_string_lossy().into_owned());
            }
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                stack.push(path);
            }
        }
    }
    out.sort();
    out
}

/// "Never extract to a path that escapes the destination
/// directory."
///
/// Checked as a statement about the *tree around* the destination rather than
/// about one filename: an escape lands in the destination's parent, or its
/// grandparent, or anywhere else the archive's `..` sequence reached. So the
/// whole fixture root is walked and every name the hostile archive could have
/// written is looked for.
fn assert_nothing_escaped(tree: &Tree, escapee: &str) {
    let outside: Vec<String> = walk(tree.root())
        .into_iter()
        .filter(|rel| rel.contains(escapee))
        .filter(|rel| !rel.starts_with("dst/"))
        .collect();
    assert!(
        outside.is_empty(),
        "{escapee} escaped the destination and landed at {outside:?} \
         under {}",
        tree.root().display()
    );
    // And the two places a `../..` most obviously aims at, named explicitly so
    // a failure says which one.
    for parent in [tree.root().to_path_buf(), tree.src(), tree.dst()] {
        let path = parent.join(escapee);
        assert!(!path.exists(), "an escaping entry wrote {}", path.display());
    }
}

// ---------------------------------------------------------------------------
// The pty session
// ---------------------------------------------------------------------------

/// How to start one `hcmd`.
struct Launch {
    cols: u16,
    rows: u16,
    cwd: PathBuf,
    /// The `[archive]` table this run gets, already rendered as TOML.
    config: String,
}

impl Launch {
    /// A default-sized session with the archive temp directory pinned at
    /// `temp`.
    fn new(cwd: impl Into<PathBuf>, temp: &Path) -> Self {
        Self {
            cols: 160,
            rows: 32,
            cwd: cwd.into(),
            config: format!("[archive]\ntemp_dir = \"{}\"\n", temp.display()),
        }
    }

    /// A wider screen.
    ///
    /// The panel status line and the failure summary both **crop**, and
    /// the refusals are long sentences whose point is the entry
    /// name in the middle of them - `1 entry was refused as unsafe to extract:
    /// ../../escape.txt: rejected - a `..` component` is 102
    /// cells, and the status line crops from the middle. 240 columns is what
    /// makes the whole sentence readable, and reading it is the criterion.
    fn wide(mut self) -> Self {
        self.cols = 240;
        self
    }

    /// Add lines to the `[archive]` table - the two thresholds.
    fn with_archive(mut self, lines: &str) -> Self {
        self.config.push_str(lines);
        self
    }
}

/// A running `hcmd`, the screen it has painted, and the throwaway `$HOME`-ish
/// directory it was given.
struct Session {
    #[expect(dead_code, reason = "held so the pty outlives the reader thread")]
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    rx: mpsc::Receiver<Vec<u8>>,
    parser: vt100::Parser,
    /// Every byte the child ever wrote, for the restore assertions.
    raw: Vec<u8>,
    home: PathBuf,
}

impl Session {
    fn start(launch: Launch) -> Self {
        let Launch {
            cols,
            rows,
            cwd,
            config,
        } = launch;

        let home = std::env::temp_dir().join(format!(
            "hcmd-ar-home-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(home.join("holoscommander")).expect("throwaway config dir");
        fs::write(home.join("holoscommander/config.toml"), config).expect("write config");

        let pair = native_pty_system()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("open pty");

        let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_hcmd"));
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        // Pin the locale so `ui.ascii_borders` resolves the same way on every
        // machine: the row splitting reads `│` and the status line reads `…`.
        cmd.env("LANG", "en_US.UTF-8");
        cmd.env("LC_ALL", "en_US.UTF-8");
        cmd.env("HCMD_KEYBOARD_PROTOCOL", "enhanced");
        cmd.env("HCMD_NO_FS_WATCH", "1");
        cmd.env("XDG_CONFIG_HOME", &home);
        cmd.env("XDG_STATE_HOME", &home);
        cmd.env("XDG_DATA_HOME", &home);
        cmd.cwd(&cwd);

        let child = pair.slave.spawn_command(cmd).expect("spawn hcmd");
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader().expect("pty reader");
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        std::thread::spawn(move || {
            let mut buf = [0_u8; 8192];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                    return;
                }
            }
        });
        let writer = pair.master.take_writer().expect("pty writer");

        Self {
            master: pair.master,
            child,
            writer,
            rx,
            parser: vt100::Parser::new(rows, cols, 0),
            raw: Vec::new(),
            home,
        }
    }

    // -- reading -----------------------------------------------------------

    fn pump(&mut self, budget: Duration) {
        let deadline = Instant::now() + budget;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return;
            }
            match self.rx.recv_timeout(left) {
                Ok(chunk) => {
                    self.raw.extend_from_slice(&chunk);
                    self.parser.process(&chunk);
                }
                Err(_) => return,
            }
        }
    }

    fn text(&self) -> String {
        self.parser.screen().contents()
    }

    fn raw_text(&self) -> String {
        String::from_utf8_lossy(&self.raw).into_owned()
    }

    fn snapshot(&self) -> (String, (u16, u16), bool) {
        let screen = self.parser.screen();
        (
            screen.contents(),
            screen.cursor_position(),
            screen.hide_cursor(),
        )
    }

    /// Poll until the rendered screen has been unchanged for a beat.
    fn settle(&mut self) {
        let mut last = self.snapshot();
        let mut stable_since = Instant::now();
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            self.pump(Duration::from_millis(60));
            let now = self.snapshot();
            if now == last {
                if stable_since.elapsed() >= Duration::from_millis(250) {
                    return;
                }
            } else {
                last = now;
                stable_since = Instant::now();
            }
        }
    }

    /// Wait until the screen has settled *and* `pred` holds, or fail loudly.
    fn wait(&mut self, what: &str, pred: impl Fn(&Self) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(90);
        loop {
            self.settle();
            if pred(self) {
                return;
            }
            if Instant::now() >= deadline {
                panic!(
                    "timed out waiting for {what}\n--- screen ---\n{}",
                    self.text()
                );
            }
            self.pump(Duration::from_millis(150));
        }
    }

    fn wait_for(&mut self, what: &str, pred: impl Fn(&str) -> bool) {
        self.wait(what, |s| pred(&s.text()));
    }

    /// Wait for a predicate **without** waiting for the screen to settle.
    ///
    /// A panel filling from an archive index and a progress dialog both repaint
    /// continuously, so [`Session::settle`] never returns while either is
    /// running - and criteria 2 and 11 have to press a key, or read the screen,
    /// during exactly that.
    fn wait_now(&mut self, what: &str, pred: impl Fn(&Self) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(90);
        loop {
            self.pump(Duration::from_millis(20));
            if pred(self) {
                return;
            }
            if Instant::now() >= deadline {
                panic!(
                    "timed out waiting (unsettled) for {what}\n--- screen ---\n{}",
                    self.text()
                );
            }
        }
    }

    // -- writing -----------------------------------------------------------

    fn send(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).expect("write to the pty");
        self.writer.flush().expect("flush the pty");
    }

    fn press(&mut self, bytes: &[u8], what: &str, pred: impl Fn(&str) -> bool) {
        self.send(bytes);
        self.wait_for(what, pred);
    }

    /// Type a string one byte at a time, then let the screen catch up.
    fn type_text(&mut self, text: &str) {
        self.send(text.as_bytes());
        self.settle();
    }

    // -- reading the painted cells ----------------------------------------

    /// The row index of the command line's caret. Everything the panels draw is
    /// above it; the key bar is below.
    fn cmdline_row(&self) -> u16 {
        let (rows, _) = self.parser.screen().size();
        let text = self.text();
        let lines: Vec<&str> = text.lines().collect();
        let last = rows.saturating_sub(1);
        let has_keybar = lines
            .get(usize::from(last))
            .is_some_and(|line| line.contains("F10"));
        if has_keybar {
            last.saturating_sub(1)
        } else {
            last
        }
    }

    /// The entry the **focused** panel's cursor bar is sitting on.
    ///
    /// Both panels draw a cursor at all times but only the
    /// focused one draws it in `panel.cursor_bg`, so this follows focus from
    /// panel to panel without being told which side it is on - which is what
    /// the criteria that drive the other panel into an archive need.
    fn cursor_row(&self) -> String {
        let screen = self.parser.screen();
        let (_, cols) = screen.size();
        let rows = self.cmdline_row();
        let want = vt100::Color::Rgb(
            paint::CURSOR_FOCUSED.0,
            paint::CURSOR_FOCUSED.1,
            paint::CURSOR_FOCUSED.2,
        );
        // Only the rows above the command line: the key bar paints its labels
        // in the same colour, and a scan of the whole screen would report the
        // key bar as a cursor bar.
        for row in 0..rows {
            let mut text = String::new();
            let mut hit = false;
            for col in 0..cols {
                let Some(cell) = screen.cell(row, col) else {
                    continue;
                };
                if cell.bgcolor() == want {
                    hit = true;
                    text.push_str(cell.contents());
                }
            }
            if hit {
                return text.trim().to_string();
            }
        }
        String::new()
    }

    /// Which half of the screen the focused cursor bar is painted in.
    fn cursor_is_left(&self) -> bool {
        let screen = self.parser.screen();
        let (_, cols) = screen.size();
        let rows = self.cmdline_row();
        let want = vt100::Color::Rgb(
            paint::CURSOR_FOCUSED.0,
            paint::CURSOR_FOCUSED.1,
            paint::CURSOR_FOCUSED.2,
        );
        for row in 0..rows {
            for col in 0..cols {
                if screen
                    .cell(row, col)
                    .is_some_and(|cell| cell.bgcolor() == want)
                {
                    return col < cols / 2;
                }
            }
        }
        false
    }

    // -- driving -----------------------------------------------------------

    /// Walk the focused panel's cursor onto the entry whose row contains
    /// `needle`.
    ///
    /// Deliberately arrow keys and not a quick search: the design binds `-`
    /// to the unmark-by-mask prompt, so a filename cannot be typed at a panel
    /// without risking a dialog, and this has to work for any name. It goes to
    /// `[..]` first so the direction is never wrong, and checks the painted
    /// cursor bar between steps - cursor movement changes no text, so "the
    /// screen settled" alone would let the next key act on the wrong row.
    fn focus_entry(&mut self, needle: &str) {
        for _ in 0..200 {
            self.settle();
            if self.cursor_row().contains("[..]") {
                break;
            }
            self.send(keys::UP);
        }
        for _ in 0..200 {
            self.settle();
            if self.cursor_row().contains(needle) {
                return;
            }
            self.send(keys::DOWN);
        }
        panic!(
            "the cursor never reached {needle:?}\n--- screen ---\n{}",
            self.text()
        );
    }

    /// Point the **other** panel at `dir` with `Ctrl+G`, then come back.
    ///
    /// the design pre-fills the copy dialog's target with "the other panel's
    /// path", so most of this milestone needs two panels pointing at different
    /// places, and `Ctrl+G` is the keyboard route to that.
    fn point_other_panel_at(&mut self, dir: &Path) {
        let shown = dir.display().to_string();
        // Waited for by its last component, not by the whole path. A long
        // $TMPDIR - macOS spells it /var/folders/<22 chars>/T/ - overflows
        // both the Go to field and the panel header, and what is painted is
        // then an elided form of the path rather than the path. Waiting for
        // the whole string would hang until the timeout on a machine whose
        // temporary directory is merely long, which is not a defect in
        // anything this test is about.
        let leaf = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| shown.clone());
        self.send(keys::TAB);
        self.press(keys::CTRL_G, "the Go to prompt", |t| t.contains("Go to"));
        self.type_text(&shown);
        let typed = leaf.clone();
        self.wait("the destination in the Go to prompt", move |s| {
            s.text().contains(&typed)
        });
        self.send(keys::ENTER);
        let landed = leaf;
        self.wait("the other panel at the destination", move |s| {
            let t = s.text();
            !t.contains("Go to") && t.contains(&landed)
        });
        self.send(keys::TAB);
        self.wait("focus to come back", Self::cursor_is_left);
    }

    /// Drive the **other** panel into the archive `name` inside `dir`, and come
    /// back.
    ///
    /// This is how `F5` *into* an archive is set up: the design seeds the
    /// copy dialog's target from the other panel's path, and inside an archive
    /// that path is `…/name#/`.
    ///
    /// `name` is the archive's whole file name. The cursor is walked onto its
    /// **stem**, because the panel draws `Name` and `Ext` as separate columns
    /// and the joined filename therefore never appears
    /// contiguously on a row.
    fn point_other_panel_into(&mut self, dir: &Path, name: &str) {
        let shown = dir.display().to_string();
        // By its last component, for the reason given on
        // [`Session::point_other_panel_at`]: a long enough path is painted
        // elided, and waiting for the whole of it hangs on a machine whose
        // only peculiarity is a long temporary directory.
        let leaf = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| shown.clone());
        self.send(keys::TAB);
        self.press(keys::CTRL_G, "the Go to prompt", |t| t.contains("Go to"));
        self.type_text(&shown);
        self.send(keys::ENTER);
        self.wait("the other panel at the archive's directory", move |s| {
            let t = s.text();
            !t.contains("Go to") && t.contains(&leaf)
        });
        self.focus_entry(name.split('.').next().unwrap_or(name));
        let inside = format!("{name}#/");
        self.press(
            keys::ENTER,
            "the other panel inside the archive",
            move |t| t.contains(&inside),
        );
        self.send(keys::TAB);
        self.wait("focus to come back", Self::cursor_is_left);
    }

    fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    fn wait_exit(&mut self, budget: Duration) -> Option<bool> {
        let deadline = Instant::now() + budget;
        while Instant::now() < deadline {
            self.pump(Duration::from_millis(100));
            if let Ok(Some(status)) = self.child.try_wait() {
                self.pump(Duration::from_millis(400));
                return Some(status.success());
            }
        }
        None
    }

    /// The terminal is back the way the user left it.
    fn assert_terminal_restored(&self, what: &str) {
        assert!(
            !self.parser.screen().alternate_screen(),
            "{what}: the alternate screen was not left"
        );
        let raw = self.raw_text();
        let entered = raw.find(restore::ALT_ON);
        let left = raw.rfind(restore::ALT_OFF);
        let shown = raw.rfind(restore::CURSOR_SHOWN);
        assert!(
            entered.is_some(),
            "{what}: the alternate screen was never entered"
        );
        assert!(
            left > entered,
            "{what}: the alternate screen was entered and not left"
        );
        // `Term::restore` shows the cursor only after `disable_raw_mode`
        // returns and writes nothing after it, so this byte arriving last is
        // the observable proof that raw mode was turned off.
        assert!(
            shown > left,
            "{what}: the cursor was not shown after disable_raw_mode"
        );
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.home);
    }
}

/// The session cache directories under `base`, by name.
fn caches(base: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(base) else {
        return Vec::new();
    };
    let mut out: Vec<String> = entries
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("hcmd-archive-"))
        .collect();
    out.sort();
    out
}

// ---------------------------------------------------------------------------
// 1. Enter browses a .zip; the path says so; Backspace comes back to it
// ---------------------------------------------------------------------------

/// "Archives are directories. `Enter` on `foo.zip` enters it; the
/// panel path shows `…/foo.zip#/`."
///
/// And the way out: `Backspace` is the parent navigation, so leaving
/// an archive has to land the cursor back on the archive that was being
/// browsed, exactly as leaving a directory lands on the directory.
#[test]
fn criterion_1_enter_browses_a_zip_and_backspace_comes_back_to_it() {
    let t = Tree::new("c1");
    let archive = t.src().join("box.zip");
    write_zip(
        &archive,
        &[
            ("alpha.txt", b"alpha, inside the zip"),
            ("sub/beta.txt", b"beta, one level down"),
        ],
    );
    // A neighbour, so "the panel came back" is a claim about a listing with
    // rows in it rather than about an empty one.
    t.file("src/zzz.txt", b"a neighbour");

    let mut s = Session::start(Launch::new(t.src(), &t.temp()));
    s.wait_for("the source listing", |t| {
        t.contains("box") && t.contains("zzz")
    });
    s.focus_entry("box");

    s.press(keys::ENTER, "the archive listing", |t| {
        t.contains("box.zip#/") && t.contains("alpha")
    });

    // the own spelling, read off the panel's top border.
    let inside = s.text();
    assert!(
        inside.contains("box.zip#/"),
        "the panel path shows `…/foo.zip#/`:\n{inside}"
    );
    // The members are rows, and `sub/` - which the container has no entry of
    // its own for - is a directory the index synthesised so the tree is
    // navigable.
    assert!(
        inside.contains("alpha") && inside.contains("[sub]"),
        "the archive's members are the listing, `sub` included:\n{inside}"
    );
    // A backend sends its own `..` row, and it is the way out.
    assert!(
        inside.contains("[..]"),
        "an archive root has a `..`:\n{inside}"
    );

    s.press(keys::BACKSPACE, "the panel back outside the archive", |t| {
        !t.contains("box.zip#/") && t.contains("zzz")
    });
    assert!(
        s.cursor_row().contains("box"),
        "leaving an archive lands the cursor back on it, got {:?}:\n{}",
        s.cursor_row(),
        s.text()
    );
    // Leaving read nothing and wrote nothing: the container is untouched.
    assert_eq!(zip_names(&archive), vec!["alpha.txt", "sub/beta.txt"]);
    assert!(s.is_running());
}

// ---------------------------------------------------------------------------
// 2. A .tar.gz lists as it decompresses, rather than after it
// ---------------------------------------------------------------------------

/// "A 4 GB `.tar.gz` must not be decompressed in full to list
/// it; build the index in the background and populate the panel as entries
/// appear."
///
/// The observable form of that: **members on screen while the panel still says
/// `reading…`**. The status line's counts wait for the listing to finish
/// (the design - a partial total is a wrong total), so `reading…` is exactly
/// "the listing is not complete", and a row of the archive's own beside it is
/// exactly "and it is already showing me things".
///
/// An implementation that read the container to the end before answering could
/// not produce that screen at all: it would show `reading…` with nothing under
/// it, and then everything at once.
#[test]
fn criterion_2_a_compressed_tar_lists_while_it_is_still_being_read() {
    let t = Tree::new("c2");
    let archive = t.src().join("payload.tar.gz");
    write_streaming_tar_gz(&archive);

    let mut s = Session::start(Launch::new(t.src(), &t.temp()));
    // `Name` and `Ext` are separate columns, so the joined
    // filename never appears contiguously on screen.
    s.wait_for("the source listing", |t| t.contains("payload"));
    s.focus_entry("payload");
    s.send(keys::ENTER);

    // Deliberately `wait_now`: a panel filling from an index repaints
    // continuously, so waiting for the screen to settle would only ever look
    // once the whole 900 MiB had been walked - which is the one moment this
    // criterion is not about.
    // Wait for the condition this criterion actually asserts, not for a weaker
    // one that precedes it. This used to wait for `member-0000` alone and then
    // demand four rows a moment later, which is a race between the first row
    // being drawn and the fourth: on a slow runner the sample landed with one
    // row on screen and the test failed while the panel was filling correctly.
    s.wait_now(
        "several members on screen while the index is still building",
        |s| {
            let text = s.text();
            let listed = (0..8)
                .filter(|i| text.contains(&format!("member-000{i}")))
                .count();
            listed >= 4 && text.contains("reading")
        },
    );

    let mid = s.text();
    assert!(
        mid.contains("payload.tar.gz#/"),
        "the panel path shows the archive:\n{mid}"
    );
    // Several rows, not one: the panel is being *populated*, not shown a single
    // teaser row.
    let listed = (0..8)
        .filter(|i| mid.contains(&format!("member-000{i}")))
        .count();
    assert!(
        listed >= 4,
        "the panel filled as the index reached entries, got {listed} of the \
         first eight members:\n{mid}"
    );

    // And the way out still works while the index runs.
    s.press(keys::BACKSPACE, "the panel back outside the archive", |t| {
        !t.contains("payload.tar.gz#/")
    });
    assert!(s.is_running());
}

// ---------------------------------------------------------------------------
// 3. F3 views a member, through the same trait everything else uses
// ---------------------------------------------------------------------------

/// the viewer reads through [`Vfs::open_read`], so a file inside an
/// archive opens in it with no archive-specific code anywhere in `viewer/`.
///
/// This is the criterion that proves the trait was worth having. It asserts the
/// viewer's *content*, in order, from the first line - a viewer that opened the
/// wrong thing, or that dropped a line, would pass a bare `contains`.
#[test]
fn criterion_3_f3_views_a_file_inside_an_archive() {
    let t = Tree::new("c3");
    let body: String = (1..=80)
        .map(|n| format!("line {n:03} of the member inside the archive\n"))
        .collect();
    write_zip(
        &t.src().join("box.zip"),
        &[("notes.txt", body.as_bytes()), ("other.txt", b"unrelated")],
    );

    let mut s = Session::start(Launch::new(t.src(), &t.temp()));
    s.wait_for("the source listing", |t| t.contains("box"));
    s.focus_entry("box");
    s.press(keys::ENTER, "the archive listing", |t| {
        t.contains("box.zip#/") && t.contains("notes")
    });
    s.focus_entry("notes");

    s.press(keys::F3, "the viewer over the member", |t| {
        t.contains("notes.txt") && t.contains("line 001")
    });

    // the viewer takes the whole screen, so the panels are gone.
    let shown = s.text();
    assert!(
        !shown.contains("other"),
        "the viewer takes the whole screen:\n{shown}"
    );
    for n in 1..=20 {
        let want = format!("line {n:03} of the member inside the archive");
        assert!(
            shown.contains(&want),
            "the viewer shows the member's own bytes, in order; {want:?} is missing:\n{shown}"
        );
    }

    s.press(keys::ESC, "the panel to come back", |t| {
        t.contains("box.zip#/") && t.contains("other")
    });
    assert!(
        s.cursor_row().contains("notes"),
        "Esc returns to the panel with the cursor on what was viewed, got {:?}",
        s.cursor_row()
    );
}

// ---------------------------------------------------------------------------
// 4. F5 out of an archive extracts, byte for byte
// ---------------------------------------------------------------------------

/// "`F5` **out of** an archive extracts, with full progress and
/// conflict handling."
///
/// The bytes are what is asserted. A 64 KiB member of pseudo-random content is
/// several `copy::COPY_CHUNK`s and compresses to nothing in particular, so
/// "byte for byte" is a claim about a real payload rather than about a run of
/// zeroes that any bug would reproduce.
#[test]
fn criterion_4_f5_extracts_a_member_to_the_other_panel_byte_for_byte() {
    let t = Tree::new("c4");
    let bytes = payload(64 * 1024, 0x51ce);
    write_zip(
        &t.src().join("box.zip"),
        &[
            ("payload.bin", &bytes),
            ("other.txt", b"the member that stays where it is"),
        ],
    );

    let mut s = Session::start(Launch::new(t.src(), &t.temp()));
    s.wait_for("the source listing", |t| t.contains("box"));
    s.point_other_panel_at(&t.dst());

    s.focus_entry("box");
    s.press(keys::ENTER, "the archive listing", |t| {
        t.contains("box.zip#/") && t.contains("payload")
    });
    s.focus_entry("payload");

    s.press(keys::F5, "the copy dialog", |t| {
        t.contains("Copy 1 file(s)")
    });
    s.press(keys::ENTER, "the extraction to finish", |t| {
        t.contains("copied 1 file")
    });

    // The screen said so; the disk is the claim.
    assert_eq!(
        listing(&t.dst()),
        vec!["payload.bin".to_string()],
        "F5 extracted the member under the cursor and only that one"
    );
    assert_eq!(
        read(&t.dst().join("payload.bin")),
        bytes,
        "the member arrived byte for byte"
    );
    // Extraction is a copy: the archive still holds everything it held.
    assert_eq!(
        zip_names(&t.src().join("box.zip")),
        vec!["payload.bin", "other.txt"]
    );
}

// ---------------------------------------------------------------------------
// 5. F5 into a .zip adds a member, and it is really in the archive
// ---------------------------------------------------------------------------

/// "`F5` **into** an archive adds, only for backends whose
/// `Capabilities` report writability. A `.zip` supports in-place member
/// addition."
///
/// Asserted by re-opening the container with `zip` afterwards. `copied 1 file`
/// on the screen and a member in the central directory are two different
/// claims, and it is the second one the design is making.
#[test]
fn criterion_5_f5_adds_a_file_into_a_zip() {
    let t = Tree::new("c5");
    let note = b"a note added from the other panel".to_vec();
    t.file("src/note.txt", &note);
    let archive = t.src().join("box.zip");
    write_zip(&archive, &[("existing.txt", b"was here first")]);

    let mut s = Session::start(Launch::new(t.src(), &t.temp()));
    s.wait_for("the source listing", |t| {
        t.contains("note") && t.contains("box")
    });
    // The *other* panel goes inside the archive: the design seeds the copy
    // dialog's target from it, and that is what makes `F5` an add rather than
    // a local copy.
    s.point_other_panel_into(&t.src(), "box.zip");

    s.focus_entry("note");
    s.press(keys::F5, "the copy dialog", |t| {
        t.contains("Copy 1 file(s)")
    });
    s.press(keys::ENTER, "the add to finish", |t| {
        t.contains("copied 1 file")
    });

    let names = zip_names(&archive);
    assert!(
        names.contains(&"note.txt".to_string()),
        "the member is really in the archive afterwards, got {names:?}"
    );
    assert!(
        names.contains(&"existing.txt".to_string()),
        "and adding one member did not lose the others, got {names:?}"
    );
    assert_eq!(
        zip_member(&archive, "note.txt"),
        note,
        "the added member holds the source's bytes"
    );
    assert_eq!(
        zip_member(&archive, "existing.txt"),
        b"was here first",
        "and the member that was already there is unchanged"
    );
    // A copy leaves its source where it was.
    assert_eq!(read(&t.src().join("note.txt")), note);
}

// ---------------------------------------------------------------------------
// 6. F5 into a .rar is refused up front, by Capabilities
// ---------------------------------------------------------------------------

/// "a read-only backend causes `F5` *into* it to be refused up
/// front with a clear message rather than failing halfway through a copy", and
/// the table: `.rar` is `yes` to read and `no` to write, for ever.
///
/// **Up front** is the whole criterion, and it is asserted three ways: the
/// message names the reason, nothing was copied, and the container is
/// byte-identical to what it was - a refusal discovered on the way through
/// would have opened it for writing first.
#[test]
fn criterion_6_f5_into_a_rar_is_refused_before_any_work() {
    let t = Tree::new("c6");
    let archive = t.src().join("stuff.rar");
    write_rar(&archive, &[("inside.txt", b"already in the rar")]);
    let before = read(&archive);
    t.file("src/note.txt", b"a note that must not reach the rar");

    let mut s = Session::start(Launch::new(t.src(), &t.temp()).wide());
    s.wait_for("the source listing", |t| {
        t.contains("stuff") && t.contains("note")
    });
    s.point_other_panel_into(&t.src(), "stuff.rar");

    // The other panel is *inside* the rar, so its members listed: the refusal
    // below is about writing, not about reading, which is the design's
    // whole point.
    assert!(
        s.text().contains("inside"),
        "the rar listed its members (read yes):\n{}",
        s.text()
    );

    s.focus_entry("note");
    s.press(keys::F5, "the copy dialog", |t| {
        t.contains("Copy 1 file(s)")
    });
    s.press(keys::ENTER, "the refusal", |t| t.contains("read-only"));

    let shown = s.text();
    assert!(
        shown.contains("nothing was written"),
        "the refusal says so in as many words:\n{shown}"
    );
    assert!(
        !shown.contains("copied 1 file"),
        "nothing was copied:\n{shown}"
    );
    assert_eq!(
        read(&archive),
        before,
        "the container is byte-identical: the refusal came before any work \
"
    );
}

// ---------------------------------------------------------------------------
// 7. Alt+F6 unpacks the archive under the cursor into the other panel
// ---------------------------------------------------------------------------

/// "`Alt+F6` unpacks the archive under the cursor to the other
/// panel's directory."
///
/// The *contents* go into the destination, not a directory named after the
/// archive: the archive's root has no file name, which is what tells the copy
/// engine which of the two it is - the rule `cp -r /` follows.
#[test]
fn criterion_7_alt_f6_unpacks_into_the_other_panels_directory() {
    let t = Tree::new("c7");
    let three = payload(9000, 0xbeef);
    write_zip(
        &t.src().join("bundle.zip"),
        &[("one.txt", b"member one"), ("two/three.bin", &three)],
    );

    let mut s = Session::start(Launch::new(t.src(), &t.temp()));
    s.wait_for("the source listing", |t| t.contains("bundle"));
    s.point_other_panel_at(&t.dst());
    s.focus_entry("bundle");

    s.send(keys::ALT_F6);
    // `Alt+F6` asks where before it writes anything; the other panel's
    // directory is already the answer, so this accepts it.
    s.wait_for("the unpack destination prompt", |t| t.contains("Unpack "));
    s.send(keys::ENTER);
    s.wait_for("the unpack to finish", |t| t.contains("copied 2 files"));

    assert_eq!(
        walk(&t.dst()),
        vec![
            "one.txt".to_string(),
            "two".to_string(),
            "two/three.bin".to_string(),
        ],
        "Alt+F6 put the archive's *contents* into the other panel's directory \
"
    );
    assert_eq!(read(&t.dst().join("one.txt")), b"member one");
    assert_eq!(
        read(&t.dst().join("two/three.bin")),
        three,
        "the nested member arrived byte for byte"
    );
    // Unpacking is a copy: the container is still there and still whole.
    assert_eq!(
        zip_names(&t.src().join("bundle.zip")),
        vec!["one.txt", "two/three.bin"]
    );
}

// ---------------------------------------------------------------------------
// 8. Alt+F5 packs the marked selection, and the archive opens
// ---------------------------------------------------------------------------

/// "`Alt+F5` packs the selection: a dialog for target name,
/// format, compression level, and 'move to archive'."
///
/// Two claims, and both are checked: the archive holds the marked set and only
/// the marked set, and it then **opens** - `Enter` on it browses it, which is
/// the own definition of an archive being one.
#[test]
fn criterion_8_alt_f5_packs_the_marked_selection_into_an_archive_that_opens() {
    let t = Tree::new("c8");
    let stuff = t.dir("src/stuff");
    let alpha = b"alpha, to be packed".to_vec();
    let bravo = payload(4096, 0x9e37);
    t.file("src/stuff/alpha.txt", &alpha);
    t.file("src/stuff/bravo.bin", &bravo);
    t.file("src/stuff/charlie.txt", b"charlie, left behind");

    let mut s = Session::start(Launch::new(&stuff, &t.temp()));
    s.wait_for("the source listing", |t| {
        t.contains("alpha") && t.contains("charlie")
    });
    s.point_other_panel_at(&t.dst());

    // `Insert` marks and steps down, and sizes nothing.
    s.focus_entry("alpha");
    s.send(keys::INSERT);
    s.press(keys::INSERT, "two entries marked", |t| {
        t.contains("2 of 3 selected")
    });

    s.press(keys::ALT_F5, "the pack dialog", |t| {
        t.contains("Pack 2 file(s)")
    });
    // The target opens on the other panel's directory and the name of what is
    // being packed - here the directory's, because there is more than one item.
    let target = t.dst().join("stuff.zip");
    let shown = target.display().to_string();
    // The field scrolls, so the whole path is not always on screen: macOS
    // spells $TMPDIR as /var/folders/<22 chars>/T/, which does not fit in a
    // dialog sized for the terminal. What is asserted is that the visible text
    // is the *tail* of the intended path, which holds at any width, rather
    // than that the path is short enough to fit, which is not the criterion.
    let screen = s.text();
    let field = screen
        .lines()
        .find(|line| line.contains("stuff.zip"))
        .map(|line| {
            line.trim_matches(|c| c == '\u{2502}' || c == ' ')
                .to_string()
        })
        .unwrap_or_else(|| panic!("no target field on screen:\n{screen}"));
    assert!(
        field.ends_with("dst/stuff.zip") && shown.ends_with(&field),
        "the dialog opens on the other panel's directory and a name taken from \
         the selection, expected the tail of {shown}, got {field:?}:\n{screen}"
    );
    s.send(keys::ENTER);
    s.wait_for("the pack to finish", |t| t.contains("copied 2 files"));

    assert_eq!(
        zip_names(&target),
        vec!["alpha.txt", "bravo.bin"],
        "the archive holds the marked set and only the marked set"
    );
    assert_eq!(zip_member(&target, "alpha.txt"), alpha);
    assert_eq!(zip_member(&target, "bravo.bin"), bravo);
    // A pack without "move to archive" leaves the sources alone.
    assert_eq!(
        listing(&stuff),
        vec![
            "alpha.txt".to_string(),
            "bravo.bin".to_string(),
            "charlie.txt".to_string()
        ]
    );

    // "...that then opens correctly": the definition of an archive.
    s.send(keys::TAB);
    s.wait("focus on the destination panel", |s| !s.cursor_is_left());
    s.press(
        keys::CTRL_R,
        "the destination listing to show the archive",
        |t| t.contains("stuff"),
    );
    s.focus_entry("stuff");
    s.press(keys::ENTER, "the new archive to open", |t| {
        t.contains("stuff.zip#/") && t.contains("alpha") && t.contains("bravo")
    });
}

// ---------------------------------------------------------------------------
// 9. Zip Slip: nothing escapes, the entry is named, the rest still extracts
// ---------------------------------------------------------------------------

/// "Never extract to a path that escapes the destination
/// directory. Reject entries containing `..` or absolute paths - this is the
/// Zip Slip class of bug and is a real vulnerability, not a theoretical one."
///
/// Three separate claims, and a fix that got any one of them wrong would be
/// worse than no fix:
///
/// 1. **Nothing outside the destination.** Checked over the whole fixture tree,
///    not by looking for one filename in one directory.
/// 2. **The entry is named.** A refusal nobody can see is indistinguishable
///    from a member that was never in the archive.
/// 3. **The legitimate entries still extract.** An archive with one hostile
///    entry is not a hostile archive, and refusing all of it would make the
///    defence worse than the attack.
#[test]
fn criterion_9_a_zip_slip_entry_escapes_nothing_is_named_and_the_rest_extracts() {
    let t = Tree::new("c9");
    write_zip(
        &t.src().join("evil.zip"),
        &[
            ("../../escape.txt", b"this must never be written"),
            ("safe.txt", b"a legitimate entry"),
            ("dir/ok.txt", b"another legitimate entry"),
        ],
    );

    let mut s = Session::start(Launch::new(t.src(), &t.temp()).wide());
    s.wait_for("the source listing", |t| t.contains("evil"));
    s.point_other_panel_at(&t.dst());

    // The refusal is reported when the listing is built: the member never
    // enters the index, so it can never be listed, opened or extracted.
    s.focus_entry("evil");
    s.press(keys::ENTER, "the archive listing and its refusal", |t| {
        t.contains("evil.zip#/") && t.contains("refused")
    });
    let inside = s.text();
    assert!(
        inside.contains("escape.txt"),
        "the refusal names the entry:\n{inside}"
    );
    assert!(
        inside.contains("refused as unsafe to extract"),
        "and says what it refused it for:\n{inside}"
    );
    assert!(
        inside.contains("safe") && inside.contains("[dir]"),
        "and the archive's legitimate entries are still listed:\n{inside}"
    );

    // Now extract the whole thing into the other panel.
    s.press(keys::BACKSPACE, "the panel back outside the archive", |t| {
        !t.contains("evil.zip#/")
    });
    s.focus_entry("evil");
    s.send(keys::ALT_F6);
    // `Alt+F6` asks where before it writes anything; the other panel's
    // directory is already the answer, so this accepts it.
    s.wait_for("the unpack destination prompt", |t| t.contains("Unpack "));
    s.send(keys::ENTER);
    s.wait_for("the unpack to finish", |t| t.contains("copied 2 files"));

    assert_eq!(
        walk(&t.dst()),
        vec![
            "dir".to_string(),
            "dir/ok.txt".to_string(),
            "safe.txt".to_string(),
        ],
        "the archive's legitimate entries extracted, and nothing else did"
    );
    assert_eq!(read(&t.dst().join("safe.txt")), b"a legitimate entry");
    assert_nothing_escaped(&t, "escape.txt");
}

// ---------------------------------------------------------------------------
// 10. A symlink pointing out of the destination is refused on the same terms
// ---------------------------------------------------------------------------

/// The second spelling of the same attack, and the one a purely lexical check
/// misses: the *member name* `link` is impeccable, and it is the **target**
/// that leaves. `a/evil -> /etc` followed by `a/evil/passwd` writes through it,
/// and every format in the table that can store a link can store
/// that pair.
///
/// So the refusal is on the same terms as criterion 9's: nothing outside the
/// destination, the entry named, and the archive's legitimate entries still
/// extracted.
#[test]
fn criterion_10_a_symlink_leaving_the_destination_is_refused_and_named() {
    let t = Tree::new("c10");
    write_tar_gz(
        &t.src().join("linky.tar.gz"),
        &[
            TarMember::File("safe.txt", b"a legitimate entry"),
            TarMember::Link("link", "../../escape.txt"),
            TarMember::File("dir/ok.txt", b"another legitimate entry"),
        ],
    );

    let mut s = Session::start(Launch::new(t.src(), &t.temp()).wide());
    s.wait_for("the source listing", |t| t.contains("linky"));
    s.point_other_panel_at(&t.dst());
    s.focus_entry("linky");

    s.send(keys::ALT_F6);
    // `Alt+F6` asks where before it writes anything; the other panel's
    // directory is already the answer, so this accepts it.
    s.wait_for("the unpack destination prompt", |t| t.contains("Unpack "));
    s.send(keys::ENTER);
    s.wait_for("the refusal", |t| t.contains("link target"));

    let shown = s.text();
    assert!(
        shown.contains("#/link"),
        "the refusal names the entry it refused:\n{shown}"
    );
    assert!(
        shown.contains("refused"),
        "and says it was refused rather than merely skipped:\n{shown}"
    );

    // The legitimate entries came out; the link did not - and nothing else did
    // either, which is what an exact listing says and a `contains` does not: a
    // link "sanitised" into a file called `escape.txt` inside the destination
    // would pass a `contains` and fails this.
    assert_eq!(
        walk(&t.dst()),
        vec![
            "dir".to_string(),
            "dir/ok.txt".to_string(),
            "safe.txt".to_string(),
        ],
        "the archive's legitimate entries extracted, the escaping link did not, \
         and nothing was invented in its place"
    );
    assert_eq!(read(&t.dst().join("safe.txt")), b"a legitimate entry");
    assert_nothing_escaped(&t, "escape.txt");
}

// ---------------------------------------------------------------------------
// 11. the gates: refuse, warn, and never destroy the original
// ---------------------------------------------------------------------------

/// all three of its answers, against the same shape of archive
/// and three different `[archive]` tables:
///
/// 1. **"Refuse above `rewrite_max_size`. Not a warning - a refusal, with the
///    reason stated and the suggestion to extract, modify and repack
///    deliberately."**
/// 2. **"Warn between `rewrite_warn_size` and `rewrite_max_size` ... with a
///    cancel that is the default button."** So the test presses nothing but
///    `Enter`: if the default were the affirmative, the archive would change.
/// 3. **"The rewrite itself writes to a temp file and renames over the original
///    only on success, so an interrupted or failed rewrite never destroys the
///    archive."**
///
/// The thresholds are configured down to kilobytes rather than the archive
/// being inflated to hundreds of megabytes: the gate is arithmetic on a
/// configured figure, and a 600 MiB fixture would test the same comparison
/// several minutes more slowly.
#[test]
fn criterion_11_the_rewrite_gates_refuse_warn_and_never_destroy_the_original() {
    // ---- 1. above `rewrite_max_size`: refused, with the reason ------------
    {
        let t = Tree::new("c11a");
        let archive = t.src().join("big.tar.gz");
        write_tar_gz(
            &archive,
            &[TarMember::File("bulk.bin", &payload(64 * 1024, 0x1111))],
        );
        let before = read(&archive);
        t.file("src/note.txt", b"a note that must not be added");

        let mut s = Session::start(
            Launch::new(t.src(), &t.temp())
                .wide()
                .with_archive("rewrite_warn_size = \"1KiB\"\nrewrite_max_size = \"8KiB\"\n"),
        );
        s.wait_for("the source listing", |t| {
            t.contains("note") && t.contains("big")
        });
        s.point_other_panel_into(&t.src(), "big.tar.gz");
        s.focus_entry("note");
        s.press(keys::F5, "the copy dialog", |t| {
            t.contains("Copy 1 file(s)")
        });
        s.press(keys::ENTER, "the refusal", |t| {
            t.contains("Rewrite refused")
        });

        let shown = s.text();
        assert!(
            shown.contains("rewrite_max_size"),
            "the refusal names the limit it is enforcing:\n{shown}"
        );
        assert!(
            shown.contains("repack it deliberately"),
            "and makes the suggestion:\n{shown}"
        );
        assert!(
            !shown.contains("copied 1 file"),
            "a refusal is not a copy:\n{shown}"
        );
        assert_eq!(
            read(&archive),
            before,
            "nothing was touched: the gates are 'checked before \
             anything is touched'"
        );
        assert_eq!(
            tar_gz_members(&archive)
                .into_iter()
                .map(|(name, _)| name)
                .collect::<Vec<_>>(),
            vec!["bulk.bin".to_string()],
            "and the member was not added"
        );
    }

    // ---- 2. between the two: warned, and the default answer is cancel -----
    {
        let t = Tree::new("c11b");
        let archive = t.src().join("mid.tar.gz");
        write_tar_gz(
            &archive,
            &[TarMember::File("bulk.bin", &payload(64 * 1024, 0x2222))],
        );
        let before = read(&archive);
        t.file("src/note.txt", b"a note the user is asked about");

        let mut s = Session::start(
            Launch::new(t.src(), &t.temp())
                .wide()
                .with_archive("rewrite_warn_size = \"1KiB\"\nrewrite_max_size = \"1GiB\"\n"),
        );
        s.wait_for("the source listing", |t| {
            t.contains("note") && t.contains("mid")
        });
        s.point_other_panel_into(&t.src(), "mid.tar.gz");
        s.focus_entry("note");
        s.press(keys::F5, "the copy dialog", |t| {
            t.contains("Copy 1 file(s)")
        });
        s.press(keys::ENTER, "the warning", |t| {
            t.contains("Rewrite the archive?")
        });

        let shown = s.text();
        assert!(
            shown.contains("rewrites the whole archive"),
            "the warning says the whole archive will be rewritten:\n{shown}"
        );
        assert!(shown.contains("KiB"), "and names the size:\n{shown}");
        assert!(
            shown.contains("Cancel"),
            "the cancel is a button of its own, not an implied Esc:\n{shown}"
        );

        // Nothing but `Enter`. the design makes the cancel the **default**
        // button, so this must cancel - and the archive proves which button
        // took the keystroke.
        s.press(keys::ENTER, "the cancel", |t| t.contains("cancelled"));
        assert_eq!(
            read(&archive),
            before,
            "`Enter` on the warning cancelled: the cancel is the \
             default button"
        );
        assert_eq!(
            tar_gz_members(&archive)
                .into_iter()
                .map(|(name, _)| name)
                .collect::<Vec<_>>(),
            vec!["bulk.bin".to_string()],
            "and no member was added"
        );
    }

    // ---- 3. interrupted partway: the original is intact and readable ------
    {
        const BATCH: usize = 40;
        let t = Tree::new("c11c");
        let archive = t.src().join("small.tar.gz");
        let keep: Vec<(String, Vec<u8>)> = (0..3)
            .map(|i| (format!("original-{i}.bin"), payload(16 * 1024, 0x3333 + i)))
            .collect();
        write_tar_gz(
            &archive,
            &keep
                .iter()
                .map(|(name, body)| TarMember::File(name.as_str(), body.as_slice()))
                .collect::<Vec<_>>(),
        );
        let original_len = fs::metadata(&archive).expect("stat").len();

        // Many members, so the rewrite that adding them costs happens many
        // times and there is a middle to interrupt. Each `flush` is a commit
        // (the design invariant 12), so this is a sequence of
        // whole rewrites with cancellable copies between them.
        let batch = t.dir("src/batch");
        for i in 0..BATCH {
            t.file(
                &format!("src/batch/f{i:03}.bin"),
                &payload(64 * 1024, 0x4444 + i as u32),
            );
        }

        let mut s = Session::start(
            Launch::new(t.src(), &t.temp())
                .wide()
                .with_archive("rewrite_warn_size = \"1GiB\"\nrewrite_max_size = \"1GiB\"\n"),
        );
        s.wait_for("the source listing", |t| t.contains("[batch]"));
        s.point_other_panel_into(&t.src(), "small.tar.gz");
        s.focus_entry("batch");
        s.press(keys::F5, "the copy dialog", |t| {
            t.contains("Copy 1 file(s)")
        });
        s.send(keys::ENTER);

        // Partway, and not merely started: the container has been rewritten at
        // least once, so there is a rewrite in flight to interrupt. Asked of
        // the filesystem rather than of the screen, because the progress
        // dialog's file counter is the copy engine's and this is about the
        // container.
        let watched = archive.clone();
        s.wait_now(
            "the archive to have been rewritten at least once",
            move |_| {
                fs::metadata(&watched).map(|m| m.len()).unwrap_or(0)
                    > original_len.saturating_add(64 * 1024)
            },
        );
        s.send(keys::ESC);
        s.wait_for("the cancelled copy to report", |t| t.contains("cancelled"));

        // **Readable**: walked from its first header to its last, which is the
        // only honest form of that claim for a compressed tar.
        let members = tar_gz_members(&archive);
        let names: Vec<&str> = members.iter().map(|(name, _)| name.as_str()).collect();

        // **Intact**: every member that was in it before is still in it, with
        // the bytes it had.
        for (name, body) in &keep {
            let found = members
                .iter()
                .find(|(got, _)| got == name)
                .unwrap_or_else(|| panic!("{name} was lost by the interrupted rewrite: {names:?}"));
            assert_eq!(
                &found.1, body,
                "{name} survived the interrupted rewrite byte for byte"
            );
        }

        // It really was interrupted...
        let added = members
            .iter()
            .filter(|(name, _)| name.starts_with("batch/f"))
            .count();
        assert!(
            added < BATCH,
            "the copy was cancelled, so it cannot have added all {BATCH} members"
        );
        assert!(
            added > 0,
            "...and it had genuinely started, so this is a cancel mid-rewrite"
        );
        // ...and every member it did add is whole. A rewrite that renamed a
        // half-written container over the original would show up here as a
        // short member rather than as a missing one.
        for (name, body) in &members {
            // The directory member itself has no bytes of its own.
            let Some(stem) = name.strip_prefix("batch/").filter(|s| !s.is_empty()) else {
                continue;
            };
            let want = fs::read(batch.join(stem)).expect("the source of an added member");
            assert_eq!(body, &want, "{name} was added half-written");
        }
        // Nothing left beside the archive: the rewrite's temp container is
        // renamed on success and removed on failure, never abandoned.
        assert_eq!(
            listing(&t.src()),
            vec!["batch".to_string(), "small.tar.gz".to_string()],
            "an interrupted rewrite left litter beside the archive"
        );
    }
}

// ---------------------------------------------------------------------------
// 12. Content sniffing beats the extension, both ways round
// ---------------------------------------------------------------------------

/// "Detection is by content sniffing first, extension second -
/// TC users routinely have archives with wrong extensions."
///
/// Both directions matter, and only one of them is the pleasant one:
///
/// * a `.zip` named `.tar` **opens**, as a zip;
/// * a text file named `.zip` **fails, with the reason**, and the panel goes
///   back where it was rather than sitting in an empty listing of something
///   that is not a directory - the rule that an unreadable place is
///   never rendered as an empty one.
#[test]
fn criterion_12_content_sniffing_beats_the_extension_in_both_directions() {
    let t = Tree::new("c12");
    // A real zip, with a name that says tar.
    write_zip(
        &t.src().join("disguised.tar"),
        &[("hidden.txt", b"a zip in a tar's clothing")],
    );
    // A text file, with a name that says zip.
    t.file(
        "src/plain.zip",
        b"this is just text, and not a zip at all\n",
    );
    t.file("src/zzz.txt", b"a neighbour");

    let mut s = Session::start(Launch::new(t.src(), &t.temp()).wide());
    s.wait_for("the source listing", |t| {
        t.contains("disguised") && t.contains("plain")
    });

    // ---- the content wins where the name is wrong ------------------------
    s.focus_entry("disguised");
    s.press(keys::ENTER, "the disguised zip to open", |t| {
        t.contains("disguised.tar#/") && t.contains("hidden")
    });
    assert!(
        s.text().contains("hidden"),
        "a .zip named .tar still enters as a zip:\n{}",
        s.text()
    );

    s.press(keys::BACKSPACE, "the panel back outside", |t| {
        !t.contains("disguised.tar#/") && t.contains("zzz")
    });

    // ---- and the content wins where the name is a promise -----------------
    s.focus_entry("plain");
    s.press(keys::ENTER, "the failure", |t| t.contains("plain.zip"));

    let shown = s.text();
    assert!(
        !shown.contains("plain.zip#/"),
        "a text file named .zip is not entered:\n{shown}"
    );
    assert!(
        shown.contains("zzz"),
        "and the panel is back where it was, with its rows:\n{shown}"
    );
    assert!(
        s.cursor_row().contains("plain"),
        "with the cursor on the file that would not open, got {:?}",
        s.cursor_row()
    );
    // The message is the reason, carried out of the backend rather than
    // invented here - all this asserts is that it names the file and is not
    // empty.
    let status: String = shown
        .lines()
        .find(|line| line.contains("plain.zip"))
        .unwrap_or_default()
        .to_string();
    assert!(
        status.len() > "plain.zip: ".len(),
        "the failure states a reason, got {status:?}"
    );
}

// ---------------------------------------------------------------------------
// with archive work in flight
// ---------------------------------------------------------------------------

/// `F10` out of an archive whose index is still being built.
///
/// The index thread is not a tokio task and is not cancelled by the runtime
/// stopping, so this is the case where a missed restore would show up. The
/// other half is step 10: temp files "cleaned up on exit, and
/// orphans from previous sessions swept at startup".
#[test]
fn quitting_with_an_archive_open_restores_the_terminal_and_clears_the_cache() {
    let t = Tree::new("quit");
    write_streaming_tar_gz(&t.src().join("payload.tar.gz"));

    // step 10's other half: litter from a session that is gone.
    let orphan = t.temp().join("hcmd-archive-4294967294-dead");
    fs::create_dir_all(orphan.join("member")).expect("orphan");
    fs::write(orphan.join("member/leftover.bin"), b"from a dead session").expect("orphan file");

    let mut s = Session::start(Launch::new(t.src(), &t.temp()));
    s.wait_now("the panel listing", |s| s.text().contains("payload"));
    s.focus_entry("payload");
    s.send(keys::ENTER);
    s.wait_now("the archive listing", |s| {
        let text = s.text();
        text.contains("payload.tar.gz#/") || text.contains("member-0")
    });

    // The session cache exists now, and the dead session's is gone.
    let live = caches(&t.temp());
    assert!(
        live.iter().any(|n| n != "hcmd-archive-4294967294-dead"),
        "the archive session made no cache directory: {live:?}"
    );
    assert!(
        !orphan.exists(),
        "the design step 10: a previous session's temp files are swept at startup"
    );

    // Quit while the index is still running. `ui.confirm_exit` is on by
    // default, so `F10` asks first.
    s.send(keys::F10);
    s.wait_now("the quit prompt", |s| {
        s.text().contains("Quit Holos Commander?")
    });
    s.send(b"y");

    let ok = s.wait_exit(Duration::from_secs(30));
    assert_eq!(ok, Some(true), "a clean quit:\n{}", s.text());
    s.assert_terminal_restored("quitting with an archive open");

    assert!(
        caches(&t.temp()).is_empty(),
        "the design step 10: the session cache is cleaned up on exit, left {:?}",
        caches(&t.temp())
    );
}

/// `SIGTERM` while an archive index is running.
///
/// the design lists `SIGTERM` beside the panic hook: "restore on exit, on panic,
/// and on `SIGTERM`". The event loop selects on it, breaks, and unwinds - which
/// also means the session's `Drop` runs and the cache goes with it.
#[test]
fn sigterm_with_an_archive_open_restores_the_terminal_and_clears_the_cache() {
    let t = Tree::new("sigterm");
    write_streaming_tar_gz(&t.src().join("payload.tar.gz"));

    let mut s = Session::start(Launch::new(t.src(), &t.temp()));
    s.wait_now("the panel listing", |s| s.text().contains("payload"));
    s.focus_entry("payload");
    s.send(keys::ENTER);
    s.wait_now("the archive listing", |s| {
        let text = s.text();
        text.contains("payload.tar.gz#/") || text.contains("member-0")
    });
    assert!(
        !caches(&t.temp()).is_empty(),
        "the archive session made no cache directory"
    );

    // Mid-index, from outside.
    let pid = s.child.process_id().expect("the child's pid");
    // SIGTERM is 15 on every platform this program runs on; `libc` is not a
    // dependency and `kill(1)` is a signal, not a computation -
    // it transforms nothing and returns no data to the test.
    let status = std::process::Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .expect("send SIGTERM");
    assert!(status.success(), "SIGTERM was not delivered");

    let ok = s.wait_exit(Duration::from_secs(30));
    assert!(
        ok.is_some(),
        "SIGTERM did not end the process:\n{}",
        s.text()
    );
    s.assert_terminal_restored("SIGTERM with an archive open");

    assert!(
        caches(&t.temp()).is_empty(),
        "the session cache survived a SIGTERM: {:?}",
        caches(&t.temp())
    );
}
