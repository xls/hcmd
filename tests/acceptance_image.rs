//! the read-only disk images, driven through the real `hcmd`
//! binary.
//!
//! Same harness as `tests/acceptance_archive.rs` - a pty, real key bytes, a
//! `vt100` screen - and the same rule:
//!
//! > **every criterion that produces bytes asserts on the bytes.**
//!
//! A panel that says `copied 1 file` and a file on disk holding what the FAT
//! partition held are two different claims, and only the second is the
//! feature.
//!
//! # The image is built here, out of the format itself
//!
//! There is no disk image checked into the repository and none is downloaded.
//! [`mbr_image`] writes a 512-byte master boot record with two primary entries
//! and places each partition's payload at its LBA, and [`fat_volume`] formats
//! a FAT volume with `fatfs` and writes one file into it. That is the whole
//! fixture: four megabytes, built in about forty lines, and honest in the one
//! way that matters - the bytes under the second entry really are a filesystem
//! a driver can mount, not a hand-rolled imitation of one.
//!
//! Nothing here shells out to `losetup`, `mkfs` or `parted`. the design's
//! whole argument for the feature is that mounting "needs root and a loop
//! device on Linux", so a test that mounted the fixture to build it would be
//! testing a program this one is deliberately not.
//!
//! # What each criterion is for
//!
//! 1. `Enter` browses the image, and a partition is a segment of the path:
//!    `…/disk.img#/2#/`, which is what the design draws and
//!    what makes `Backspace` walk back out through the partition list.
//! 2. `F3` views a file inside a partition, through the same `Vfs` the viewer
//!    already had.
//! 3. `F5` copies one out, byte for byte.
//! 4. Marking decides what `F5` copies, inside a partition as anywhere else.
//! 5. `F6` is refused before the question, and nothing is written.

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
/// `src/event/sys/unix/parse.rs`, and the same table
/// `tests/acceptance_archive.rs` uses.
///
/// `F6` is `CSI 17 ~`, which is the key the design gives to move and the
/// one this milestone has to refuse.
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
    pub const F6: &[u8] = b"\x1b[17~";

    /// `Ctrl+G`, the Go to prompt, in the Kitty encoding the
    /// harness turns on.
    pub const CTRL_G: &[u8] = b"\x1b[103;5u";
}

/// The theme colours the assertions read back out of the rendered cells.
mod paint {
    /// `panel.cursor_bg` - the focused panel's cursor bar.
    pub const CURSOR_FOCUSED: (u8, u8, u8) = (0x00, 0xA8, 0xA8);
}

// ---------------------------------------------------------------------------
// The fixture tree
// ---------------------------------------------------------------------------

/// A throwaway `src`, `dst` and `temp` under one root.
struct Tree {
    root: PathBuf,
}

impl Tree {
    fn new(tag: &str) -> Self {
        // Deliberately short: the panel's top border carries the path
        // and crops it in the middle, and criterion 1 reads
        // `disk.img#/2#/` back off it.
        let root = std::env::temp_dir().join(format!(
            "hcmd-im-{}-{}-{tag}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).expect("fixture src");
        fs::create_dir_all(root.join("dst")).expect("fixture dst");
        fs::create_dir_all(root.join("temp")).expect("image temp dir");
        Self { root }
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
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// The names in `dir`, sorted.
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

/// Deterministic pseudo-random bytes, so "byte for byte" is a claim about a
/// real payload rather than about a run of zeroes any bug would reproduce.
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
// Building the image, out of the format itself
// ---------------------------------------------------------------------------

/// The only sector size this backend reads a partition table at.
///
const SECTOR: usize = 512;

/// One primary MBR entry, and the bytes that go at its LBA.
struct Part {
    type_byte: u8,
    start_lba: u32,
    sectors: u32,
    payload: Vec<u8>,
}

/// A FAT volume of `bytes` bytes holding `files`, formatted by `fatfs` itself.
///
/// The one place this file writes a filesystem, and it is what makes the
/// fixture honest: the partition really is mountable, so what the panel lists
/// is what a driver would list.
fn fat_volume(bytes: usize, files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut disk = std::io::Cursor::new(vec![0u8; bytes]);
    fatfs::format_volume(&mut disk, fatfs::FormatVolumeOptions::new()).expect("format");
    disk.set_position(0);
    {
        let fs = fatfs::FileSystem::new(&mut disk, fatfs::FsOptions::new()).expect("open");
        let root = fs.root_dir();
        for (name, bytes) in files {
            let mut file = root.create_file(name).expect("create");
            file.write_all(bytes).expect("write");
            file.flush().expect("flush");
        }
    }
    disk.into_inner()
}

/// Put `bytes` into `image` at `at`, growing it if it has to.
fn splice(image: &mut Vec<u8>, at: usize, bytes: &[u8]) {
    let end = at.saturating_add(bytes.len());
    if image.len() < end {
        image.resize(end, 0);
    }
    if let Some(slot) = image.get_mut(at..end) {
        slot.copy_from_slice(bytes);
    }
}

/// A disk image with a 512-byte MBR and each partition's payload at its LBA.
fn mbr_image(parts: &[Part]) -> Vec<u8> {
    let mut image = vec![0u8; SECTOR];
    for (slot, part) in parts.iter().take(4).enumerate() {
        let off = 446_usize.saturating_add(slot.saturating_mul(16));
        splice(&mut image, off.saturating_add(4), &[part.type_byte]);
        splice(
            &mut image,
            off.saturating_add(8),
            &part.start_lba.to_le_bytes(),
        );
        splice(
            &mut image,
            off.saturating_add(12),
            &part.sectors.to_le_bytes(),
        );
    }
    splice(&mut image, 510, &[0x55, 0xAA]);
    for part in parts.iter().take(4) {
        let at = (part.start_lba as usize).saturating_mul(SECTOR);
        if part.payload.is_empty() {
            // The declared window still has to be inside the file, or
            // the design I3 refuses the partition by number.
            let end = at.saturating_add((part.sectors as usize).saturating_mul(SECTOR));
            if image.len() < end {
                image.resize(end, 0);
            }
        } else {
            splice(&mut image, at, &part.payload);
        }
    }
    image
}

/// The fixture every criterion here drives: a two-partition MBR image whose
/// **second** partition is a FAT volume holding `files`.
///
/// Two partitions rather than one, and the interesting one second, so that
/// `Enter` on the row `2` is a choice among siblings rather than the only row
/// there is.
fn two_partition_image(path: &Path, files: &[(&str, &[u8])]) {
    let first = 2048_u32;
    let second = first.saturating_add(2048);
    let image = mbr_image(&[
        Part {
            // Linux, and left empty: it is here to be a sibling.
            type_byte: 0x83,
            start_lba: first,
            sectors: 2048,
            payload: Vec::new(),
        },
        Part {
            // FAT16 with LBA addressing.
            type_byte: 0x0E,
            start_lba: second,
            sectors: 4096,
            payload: fat_volume(2 * 1024 * 1024, files),
        },
    ]);
    fs::write(path, image).expect("write the disk image");
}

// ---------------------------------------------------------------------------
// The pty session
// ---------------------------------------------------------------------------

/// How to start one `hcmd`.
struct Launch {
    cols: u16,
    rows: u16,
    cwd: PathBuf,
    config: String,
}

impl Launch {
    /// A default-sized session with the session temp directory pinned at
    /// `temp`.
    ///
    /// `[archive] temp_dir` and not an `[image]` table: the design defines no
    /// configuration for disk images and none was invented, so an image that
    /// has to be materialised lands under the directory this key already names.
    ///
    fn new(cwd: impl Into<PathBuf>, temp: &Path) -> Self {
        Self {
            cols: 160,
            rows: 32,
            cwd: cwd.into(),
            config: format!("[archive]\ntemp_dir = \"{}\"\n", temp.display()),
        }
    }
}

/// A running `hcmd`, the screen it has painted, and the throwaway config
/// directory it was given.
struct Session {
    #[expect(dead_code, reason = "held so the pty outlives the reader thread")]
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    rx: mpsc::Receiver<Vec<u8>>,
    parser: vt100::Parser,
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
            "hcmd-im-home-{}-{}",
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
                Ok(chunk) => self.parser.process(&chunk),
                Err(_) => return,
            }
        }
    }

    fn text(&self) -> String {
        self.parser.screen().contents()
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

    // -- writing -----------------------------------------------------------

    fn send(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).expect("write to the pty");
        self.writer.flush().expect("flush the pty");
    }

    fn press(&mut self, bytes: &[u8], what: &str, pred: impl Fn(&str) -> bool) {
        self.send(bytes);
        self.wait_for(what, pred);
    }

    fn type_text(&mut self, text: &str) {
        self.send(text.as_bytes());
        self.settle();
    }

    // -- reading the painted cells ----------------------------------------

    /// The row index of the command line's caret. Everything the panels draw
    /// is above it; the key bar is below.
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
    fn cursor_row(&self) -> String {
        let screen = self.parser.screen();
        let (_, cols) = screen.size();
        let rows = self.cmdline_row();
        let want = vt100::Color::Rgb(
            paint::CURSOR_FOCUSED.0,
            paint::CURSOR_FOCUSED.1,
            paint::CURSOR_FOCUSED.2,
        );
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

    /// Walk the focused panel's cursor onto the row whose **name** is `name`.
    ///
    /// The first whitespace-separated token of the painted row and not a
    /// `contains`, because a partition row is named by its number and nothing
    /// else (the design J13): `1` and `2` occur in the size and
    /// date columns of every row on the screen, so a substring test would stop
    /// on the first row it drew rather than on the one that is called `2`.
    ///
    /// The panel draws `Name` and `Ext` as separate columns, so
    /// `readme.txt` is `readme` here, and it brackets a directory's name, so a
    /// partition row is `[2]` on the screen and `2` to this.
    fn focus_row_named(&mut self, name: &str) {
        for _ in 0..200 {
            self.settle();
            if self.cursor_row().contains("[..]") {
                break;
            }
            self.send(keys::UP);
        }
        for _ in 0..200 {
            self.settle();
            if self.cursor_row_name().as_deref() == Some(name) {
                return;
            }
            self.send(keys::DOWN);
        }
        panic!(
            "the cursor never reached the row named {name:?}\n--- screen ---\n{}",
            self.text()
        );
    }

    /// The name column of the row the cursor is on, unbracketed.
    fn cursor_row_name(&self) -> Option<String> {
        let row = self.cursor_row();
        let first = row.split_whitespace().next()?;
        Some(first.trim_matches(|c| c == '[' || c == ']').to_string())
    }

    /// Point the **other** panel at `dir` with `Ctrl+G`, then come back.
    fn point_other_panel_at(&mut self, dir: &Path) {
        let shown = dir.display().to_string();
        // By its last component: a long enough path is painted elided, so
        // waiting for the whole of it hangs on a machine whose only
        // peculiarity is a long temporary directory (macOS spells $TMPDIR as
        // /var/folders/<22 chars>/T/).
        let leaf = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| shown.clone());
        self.send(keys::TAB);
        self.press(keys::CTRL_G, "the Go to prompt", |t| t.contains("Go to"));
        self.type_text(&shown);
        self.send(keys::ENTER);
        self.wait("the other panel at the destination", move |s| {
            let t = s.text();
            !t.contains("Go to") && t.contains(&leaf)
        });
        self.send(keys::TAB);
        self.wait("focus to come back", Self::cursor_is_left);
    }

    /// `Enter` on the image, then `Enter` on the row `2`: the two keystrokes
    /// every criterion below starts with.
    fn enter_partition_two(&mut self, image: &str) {
        self.wait_for("the source listing", |t| t.contains("disk"));
        self.focus_row_named(image.split('.').next().unwrap_or(image));
        self.press(keys::ENTER, "the partition table", |t| {
            t.contains("disk.img#/")
        });
        self.focus_row_named("2");
        self.press(keys::ENTER, "the volume in partition 2", |t| {
            t.contains("disk.img#/2#/")
        });
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.home);
    }
}

// ---------------------------------------------------------------------------
// 1. Enter browses the image, and a partition is a segment of the path
// ---------------------------------------------------------------------------

/// "`Enter` on a disk image browses it as a directory."
///
/// The path is the assertion. the design makes a partition
/// its own segment, so partition 2 of `disk.img` is `disk.img#/2#/` and not a
/// directory called `2` inside one namespace - which is what lets `Backspace`
/// walk back out through the partition list and then out of the image, with
/// the cursor landing on what was left each time.
#[test]
fn criterion_1_enter_browses_an_image_and_a_partition_is_its_own_segment() {
    let t = Tree::new("c1");
    two_partition_image(
        &t.src().join("disk.img"),
        &[("readme.txt", b"inside partition two")],
    );

    let mut s = Session::start(Launch::new(t.src(), &t.temp()));
    s.wait_for("the source listing", |t| t.contains("disk"));
    s.focus_row_named("disk");

    s.press(keys::ENTER, "the partition table", |t| {
        t.contains("disk.img#/")
    });
    let table = s.text();
    assert!(
        table.contains("disk.img#/"),
        "the panel path shows the image's own root:\n{table}"
    );

    s.focus_row_named("1");
    s.focus_row_named("2");

    s.press(keys::ENTER, "the volume in partition 2", |t| {
        t.contains("disk.img#/2#/") && t.contains("readme")
    });
    let inside = s.text();
    assert!(
        inside.contains("disk.img#/2#/"),
        "a partition is a segment, so the path carries two `#`:\n{inside}"
    );
    assert!(
        inside.contains("readme"),
        "the FAT volume listed its root:\n{inside}"
    );

    // Out through the partition list, and then out of the image.
    s.press(keys::BACKSPACE, "the partition table again", |t| {
        t.contains("disk.img#/") && !t.contains("disk.img#/2#/")
    });
    assert_eq!(
        s.cursor_row_name().as_deref(),
        Some("2"),
        "leaving a partition lands the cursor back on its row, got {:?}",
        s.cursor_row()
    );
    s.press(keys::BACKSPACE, "the directory holding the image", |t| {
        !t.contains("disk.img#/")
    });
    assert!(
        s.cursor_row().contains("disk"),
        "leaving the image lands the cursor back on it, got {:?}",
        s.cursor_row()
    );
}

// ---------------------------------------------------------------------------
// 2. F3 views a file inside a partition
// ---------------------------------------------------------------------------

/// the trait is what makes the viewer work inside a container with
/// no container-specific code, and the design adds a third kind of one.
///
/// The viewer's *content* is asserted, in order, from the first line: a viewer
/// that opened the wrong thing, or that dropped a line, would pass a bare
/// `contains`.
#[test]
fn criterion_2_f3_views_a_file_inside_a_partition() {
    let t = Tree::new("c2");
    let body: String = (1..=60)
        .map(|n| format!("line {n:03} of the file inside partition two\n"))
        .collect();
    two_partition_image(
        &t.src().join("disk.img"),
        &[("notes.txt", body.as_bytes()), ("other.txt", b"unrelated")],
    );

    let mut s = Session::start(Launch::new(t.src(), &t.temp()));
    s.enter_partition_two("disk.img");
    s.focus_row_named("notes");

    s.press(keys::F3, "the viewer over the member", |t| {
        t.contains("notes.txt") && t.contains("line 001")
    });
    let shown = s.text();
    assert!(
        !shown.contains("other"),
        "the viewer takes the whole screen:\n{shown}"
    );
    for n in 1..=20 {
        let want = format!("line {n:03} of the file inside partition two");
        assert!(
            shown.contains(&want),
            "the viewer shows the member's own bytes, in order; {want:?} is missing:\n{shown}"
        );
    }

    s.press(keys::ESC, "the panel to come back", |t| {
        t.contains("disk.img#/2#/") && t.contains("other")
    });
}

// ---------------------------------------------------------------------------
// 3. F5 copies a file out of a partition, byte for byte
// ---------------------------------------------------------------------------

/// the design is read-only, which is a statement about writing *into* an
/// image and about nothing else: getting a file out of one is the whole point
/// of being able to open it.
///
/// The bytes are what is asserted. 64 KiB of pseudo-random content is several
/// copy chunks and several FAT clusters, so "byte for byte" is a claim about a
/// real payload and about the cluster chain being followed to its end.
#[test]
fn criterion_3_f5_copies_a_file_out_of_a_partition_byte_for_byte() {
    let t = Tree::new("c3");
    let bytes = payload(64 * 1024, 0x1ce_5eed);
    two_partition_image(
        &t.src().join("disk.img"),
        &[
            ("payload.bin", &bytes),
            ("other.txt", b"the file that stays where it is"),
        ],
    );

    let mut s = Session::start(Launch::new(t.src(), &t.temp()));
    s.wait_for("the source listing", |t| t.contains("disk"));
    s.point_other_panel_at(&t.dst());
    s.enter_partition_two("disk.img");
    s.focus_row_named("payload");

    s.press(keys::F5, "the copy dialog", |t| {
        t.contains("Copy 1 file(s)")
    });
    s.press(keys::ENTER, "the copy to finish", |t| {
        t.contains("copied 1 file")
    });

    // The screen said so; the disk is the claim.
    assert_eq!(
        listing(&t.dst()),
        vec!["payload.bin".to_string()],
        "F5 copied the file under the cursor and only that one"
    );
    let out = fs::read(t.dst().join("payload.bin")).expect("read what was copied");
    assert_eq!(
        out.len(),
        bytes.len(),
        "the whole file came out of the partition"
    );
    assert!(out == bytes, "byte for byte out of the FAT volume");
}

// ---------------------------------------------------------------------------
// 4. Marking decides what F5 copies, inside a partition too
// ---------------------------------------------------------------------------

/// the marks are a property of the panel, not of the backend, and
/// the design operates on "the marked files, or the one under the cursor if
/// none are marked". Inside a disk image both halves have to still be true.
#[test]
fn criterion_4_marking_inside_a_partition_decides_what_is_copied() {
    let t = Tree::new("c4");
    two_partition_image(
        &t.src().join("disk.img"),
        &[
            ("alpha.txt", b"the first marked file"),
            ("bravo.txt", b"the second marked file"),
            ("charlie.txt", b"the one that is not marked"),
        ],
    );

    let mut s = Session::start(Launch::new(t.src(), &t.temp()));
    s.wait_for("the source listing", |t| t.contains("disk"));
    s.point_other_panel_at(&t.dst());
    s.enter_partition_two("disk.img");

    // `Insert` marks and steps down, which is what makes two marks two keys.
    s.focus_row_named("alpha");
    s.send(keys::INSERT);
    s.settle();
    s.focus_row_named("bravo");
    s.send(keys::INSERT);
    s.settle();

    s.press(keys::F5, "the copy dialog for two files", |t| {
        t.contains("Copy 2 file(s)")
    });
    s.press(keys::ENTER, "the copy to finish", |t| {
        t.contains("copied 2 file")
    });

    assert_eq!(
        listing(&t.dst()),
        vec!["alpha.txt".to_string(), "bravo.txt".to_string()],
        "the marks decided it, and the unmarked file stayed inside the image"
    );
    assert_eq!(
        fs::read(t.dst().join("alpha.txt")).expect("read alpha"),
        b"the first marked file",
    );
    assert_eq!(
        fs::read(t.dst().join("bravo.txt")).expect("read bravo"),
        b"the second marked file",
    );
}

// ---------------------------------------------------------------------------
// 5. F6 is refused before the question, and writes nothing
// ---------------------------------------------------------------------------

/// "read-only, and not as a first step towards writing."
///
/// the design wants that refused *before the question*: `Capabilities` is what
/// the UI consults before offering an operation, "rather than failing halfway
/// through a copy". So `F6` on a file inside a partition never opens its
/// dialog at all, the status line says why, and the image on disk is unchanged
/// to the byte.
#[test]
fn criterion_5_f6_out_of_an_image_is_refused_and_nothing_is_written() {
    let t = Tree::new("c5");
    let image = t.src().join("disk.img");
    two_partition_image(
        &image,
        &[("readme.txt", b"this file is not going anywhere")],
    );
    let before = fs::read(&image).expect("read the fixture back");

    let mut s = Session::start(Launch::new(t.src(), &t.temp()));
    s.wait_for("the source listing", |t| t.contains("disk"));
    s.point_other_panel_at(&t.dst());
    s.enter_partition_two("disk.img");
    s.focus_row_named("readme");

    s.press(keys::F6, "the refusal in the status line", |t| {
        t.contains("read-only")
    });
    let shown = s.text();
    assert!(
        !shown.contains("Move 1 file(s)"),
        "the dialog was never opened, so the refusal came before the question \
:\n{shown}"
    );
    assert!(
        shown.contains("disk.img#/2#/"),
        "and the panel is still where it was:\n{shown}"
    );

    assert_eq!(
        listing(&t.dst()),
        Vec::<String>::new(),
        "nothing reached the other panel"
    );
    assert_eq!(
        fs::read(&image).expect("read the image back"),
        before,
        "the image is unchanged to the byte"
    );
}
