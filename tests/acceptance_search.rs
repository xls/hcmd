//! v0.6 driven through the real `hcmd` binary: `Alt+F7` search, `Ctrl+B` branch
//! view, `Ctrl+R` / `Esc` leaving a virtual listing, and `Ctrl+M` multi-rename.
//!
//!
//! Same harness as `tests/acceptance_ops.rs` - a pty, real key bytes, a `vt100`
//! screen - and the same rule: a milestone whose output is files on disk is
//! asserted against the disk, not against a status line that says it happened.
//! The search half has no output on disk, so it is asserted against the rows
//! the panel actually drew and the header it drew above them.
//!
//! Two things worth knowing before reading a test:
//!
//! * **The walk is asynchronous by design.** the design streams results into
//!   the panel as they are found, so every assertion goes through
//!   [`Session::wait_for`], which polls until the screen has settled *and* the
//!   thing being waited for is on it. Nothing here sleeps.
//! * **`Ctrl+M` is only distinguishable under the Kitty protocol**, where it is
//!   `ESC [ 109 ; 5 u`; on a legacy terminal it is the same byte as `Enter`.
//!   `HCMD_KEYBOARD_PROTOCOL=enhanced` is set for the same reason
//!   `tests/acceptance.rs` sets it.

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
/// `CSI <n> ; <mod> ~` is `parse_csi_special_key_code`: `18` is `F7`, and the
/// modifier field is a bitmask offset by one, so `;3` is Alt.
/// `CSI <codepoint> ; <mod> u` is the Kitty form and the codepoint is the key's
/// *unshifted* value: `Ctrl+B` is `98` (`'b'`), `Ctrl+M` is `109` (`'m'`).
mod keys {
    pub const ENTER: &[u8] = b"\r";
    pub const ESC: &[u8] = b"\x1b";
    pub const TAB: &[u8] = b"\t";
    pub const SPACE: &[u8] = b" ";

    /// `Alt+F7` - the search.
    pub const ALT_F7: &[u8] = b"\x1b[18;3~";
    /// `Ctrl+B` - the branch view.
    pub const CTRL_B: &[u8] = b"\x1b[98;5u";
    /// `Ctrl+M` - the multi-rename.
    pub const CTRL_M: &[u8] = b"\x1b[109;5u";
    /// `Ctrl+R` - reread, and on a virtual listing the leave.
    pub const CTRL_R: &[u8] = b"\x1b[114;5u";
}

// ---------------------------------------------------------------------------
// The fixture tree
// ---------------------------------------------------------------------------

/// A small tree with files at three depths, built per test and removed on drop.
///
/// Depth is the point: a search and a branch view both have to find
/// `deep/nested/buried.rs`, and a plain directory listing must not.
struct Tree {
    root: PathBuf,
}

impl Tree {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "hcmd-search-{}-{}-{tag}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("fixture root");
        Self { root }
    }

    fn root(&self) -> &Path {
        &self.root
    }

    fn file(&self, rel: &str, bytes: &[u8]) -> PathBuf {
        let path = self.root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("fixture parent");
        }
        fs::write(&path, bytes).expect("fixture file");
        path
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// The names directly inside a directory, sorted.
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

// ---------------------------------------------------------------------------
// The pty session
// ---------------------------------------------------------------------------

/// A running `hcmd`, the screen it has painted, and its throwaway config dirs.
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
    fn start(cwd: &Path, cols: u16, rows: u16) -> Self {
        let home = std::env::temp_dir().join(format!(
            "hcmd-search-home-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(home.join("holoscommander")).expect("throwaway config dir");

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
        cmd.cwd(cwd);

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

    /// The throwaway `$XDG_CONFIG_HOME`, which is also where `searches.toml`
    /// lands.
    fn config_home(&self) -> &Path {
        &self.home
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

    /// Only the **left** panel's half of the screen.
    ///
    /// The two panels split the width and the right one is still showing the
    /// real directory the search started from, so a negative assertion - "this
    /// name is not among the results" - has to be made about one panel or it
    /// reads the other one's rows and fails for the wrong reason.
    fn left_text(&self) -> String {
        let screen = self.parser.screen();
        let (rows, cols) = screen.size();
        let half = cols / 2;
        let mut out = String::new();
        for row in 0..rows {
            for col in 0..half {
                if let Some(cell) = screen.cell(row, col) {
                    out.push_str(cell.contents());
                }
            }
            out.push('\n');
        }
        out
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
    fn wait_for(&mut self, what: &str, pred: impl Fn(&str) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            self.settle();
            if pred(&self.text()) {
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

    /// The panel has finished its first read: `[..]` is drawn.
    fn wait_for_listing(&mut self) {
        self.wait_for("the first listing", |text| text.contains("[..]"));
    }

    // -- writing -----------------------------------------------------------

    fn send(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).expect("write to pty");
        self.writer.flush().expect("flush pty");
    }

    fn press(&mut self, bytes: &[u8], what: &str, pred: impl Fn(&str) -> bool) {
        self.send(bytes);
        self.wait_for(what, pred);
    }

    fn type_text(&mut self, text: &str) {
        self.send(text.as_bytes());
        self.settle();
    }

    /// Open the Find dialog and wait for it.
    fn open_find(&mut self) {
        self.press(keys::ALT_F7, "the Find files dialog", |text| {
            text.contains("Find files")
        });
    }

    /// Walk the focus ring `n` controls forward.
    fn tabs(&mut self, n: usize) {
        for _ in 0..n {
            self.send(keys::TAB);
            self.settle();
        }
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
// The criteria
// ---------------------------------------------------------------------------

/// `Alt+F7` opens the dialog, and the mask typed into it
/// feeds **the panel that was active when the search started**.
#[test]
fn alt_f7_searches_by_mask_and_feeds_the_active_panel() {
    let tree = Tree::new("mask");
    tree.file("top.rs", b"fn main() {}\n");
    tree.file("notes.txt", b"nothing\n");
    tree.file("deep/nested/buried.rs", b"fn buried() {}\n");
    tree.file("deep/nested/ignored.md", b"# no\n");

    let mut s = Session::start(tree.root(), 120, 30);
    s.wait_for_listing();
    // `buried.rs` is two directories down, so a plain listing cannot show it.
    assert!(
        !s.text().contains("buried"),
        "the fixture's deep file is visible before the search:\n{}",
        s.text()
    );

    s.open_find();
    // The mask field opens selected, so this replaces the `*` rather than
    // appending to it (the rule, which the design inherits).
    s.type_text("*.rs");
    s.press(keys::ENTER, "the search results", |text| {
        text.contains("buried")
    });

    // The panel draws the name and the extension in two columns, so every
    // assertion here is on the stem: `buried.rs` is never one contiguous
    // string on screen.
    let text = s.left_text();
    assert!(text.contains("top"), "the shallow hit is missing:\n{text}");
    assert!(
        !text.contains("notes") && !text.contains("ignored"),
        "the mask let a non-match through:\n{text}"
    );
    // the panel header says which listing this is.
    assert!(
        text.contains("[search:"),
        "the panel header does not say it is a search:\n{text}"
    );
}

/// the `Find text` checkbox and the "`Enter` opens the viewer at the
/// matching line".
#[test]
fn a_content_search_finds_by_text_and_enter_opens_the_viewer_at_the_hit() {
    let tree = Tree::new("content");
    tree.file("alpha.txt", b"one\ntwo\nNEEDLE here\nfour\n");
    tree.file("bravo.txt", b"nothing to see\n");
    tree.file("sub/charlie.txt", b"first\nsecond\nNEEDLE again\n");

    let mut s = Session::start(tree.root(), 120, 30);
    s.wait_for_listing();
    s.open_find();
    s.type_text("*.txt");
    // The dialog opens on the mask. The General tab's ring runs mask, RegEx,
    // root, `>>`, root list, Devices, restrict, archives, depth, Find text, so
    // `Find text` is nine controls further on.
    s.tabs(9);
    s.send(keys::SPACE);
    s.settle();
    // Focus is on `Find text`; the text field is the next control.
    s.send(keys::TAB);
    s.settle();
    s.type_text("NEEDLE");
    s.press(keys::ENTER, "the content hits", |text| {
        text.contains("alpha")
    });

    let text = s.left_text();
    assert!(
        text.contains("charlie"),
        "the hit in the subdirectory is missing:\n{text}"
    );
    assert!(
        !text.contains("bravo"),
        "a file without the text was reported as a hit:\n{text}"
    );

    // `Enter` on a content match "opens the viewer at the
    // matching line". A virtual listing has no `..` row, so the cursor is
    // already on a hit. Both files carry their NEEDLE on line 3, so a viewer
    // that had opened at byte 0 would say line 1 and this would fail.
    s.press(keys::ENTER, "the viewer over the hit", |text| {
        text.contains("NEEDLE") && text.contains(": line 3")
    });
}

/// the last sentence: `Ctrl+B` is the same mechanism with an empty
/// pattern - a flat recursive listing of the current tree.
#[test]
fn ctrl_b_is_a_flat_recursive_listing_of_the_tree() {
    let tree = Tree::new("branch");
    tree.file("shallow.txt", b"a\n");
    tree.file("one/two/three/very-deep.txt", b"b\n");

    let mut s = Session::start(tree.root(), 120, 30);
    s.wait_for_listing();
    s.press(keys::CTRL_B, "the branch listing", |text| {
        text.contains("very-deep")
    });

    let text = s.text();
    assert!(
        text.contains("shallow"),
        "the shallow file is missing from the branch view:\n{text}"
    );
    assert!(
        text.contains("[branch:"),
        "the header says `search` rather than `branch`:\n{text}"
    );
}

/// `Ctrl+R` on a virtual panel clears it and returns the panel
/// to its underlying real directory; `Esc` does the same thing.
#[test]
fn ctrl_r_and_esc_both_leave_a_virtual_listing() {
    let tree = Tree::new("leave");
    tree.file("here.txt", b"a\n");
    tree.file("down/there.txt", b"b\n");

    let mut s = Session::start(tree.root(), 120, 30);
    s.wait_for_listing();

    s.press(keys::CTRL_B, "the branch listing", |text| {
        text.contains("there")
    });
    s.press(keys::CTRL_R, "the real directory back", |text| {
        text.contains("left the branch listing")
    });
    let text = s.left_text();
    assert!(
        !text.contains("[branch:") && !text.contains("there"),
        "ctrl+r left the panel virtual:\n{text}"
    );
    assert!(
        text.contains("here"),
        "the real directory did not come back:\n{text}"
    );

    // And again with `Esc`. The walk has already finished, so the first `Esc`
    // is the leave rather than the stop.
    s.press(keys::CTRL_B, "the branch listing again", |text| {
        text.contains("there")
    });
    s.press(keys::ESC, "the real directory back again", |text| {
        text.contains("left the branch listing")
    });
    assert!(
        !s.text().contains("[branch:"),
        "esc left the panel virtual:\n{}",
        s.text()
    );
}

/// `Ctrl+M` renames the whole directory when nothing is marked,
/// and the files on disk are what the test believes, not the preview table.
#[test]
fn ctrl_m_renames_every_file_in_the_directory() {
    let tree = Tree::new("rename");
    tree.file("alpha.txt", b"a\n");
    tree.file("beta.txt", b"b\n");

    let mut s = Session::start(tree.root(), 130, 40);
    s.wait_for_listing();
    s.press(keys::CTRL_M, "the multi-rename dialog", |text| {
        text.contains("Rename mask")
    });

    // The name mask opens on `[N]` with the caret at its end, so this makes it
    // `[N]-v2`: the old stem, then a literal suffix. The extension mask is
    // untouched, so `.txt` survives.
    s.type_text("-v2");
    s.wait_for("the preview to show the new names", |text| {
        text.contains("alpha-v2.txt")
    });

    // `Enter` from a field is `Start!`.
    s.press(keys::ENTER, "the rename to finish", |text| {
        text.contains("renamed")
    });

    s.wait_for("the panel to show the renamed files", |text| {
        text.contains("alpha-v2")
    });
    assert_eq!(
        listing(tree.root()),
        vec!["alpha-v2.txt".to_string(), "beta-v2.txt".to_string()],
        "the files on disk are not what the rename claimed"
    );
}

/// the session undo, reached through the dialog's `Undo` button.
#[test]
fn undo_puts_a_multi_rename_back() {
    let tree = Tree::new("undo");
    tree.file("one.dat", b"1\n");
    tree.file("two.dat", b"2\n");

    let mut s = Session::start(tree.root(), 130, 40);
    s.wait_for_listing();
    s.press(keys::CTRL_M, "the multi-rename dialog", |text| {
        text.contains("Rename mask")
    });
    s.type_text("-x");
    s.press(keys::ENTER, "the rename to finish", |text| {
        text.contains("renamed")
    });
    s.wait_for("the renamed files", |text| text.contains("one-x"));
    assert_eq!(
        listing(tree.root()),
        vec!["one-x.dat".to_string(), "two-x.dat".to_string()]
    );

    // Reopen and press `Undo`, which is one control past `Start!`: fourteen
    // tabs from the name mask reach `Start!`, fifteen reach `Undo`.
    s.press(keys::CTRL_M, "the dialog again", |text| {
        text.contains("Rename mask")
    });
    s.tabs(15);
    s.press(keys::ENTER, "the undo to finish", |text| {
        text.contains("renamed")
    });
    s.wait_for("the original names", |text| {
        text.contains("one") && !text.contains("one-x")
    });
    assert_eq!(
        listing(tree.root()),
        vec!["one.dat".to_string(), "two.dat".to_string()],
        "the undo did not put the files back"
    );
}

/// the Load/Save tab: `Save as…` writes `searches.toml` into the
/// **config** directory, and the write happens in the event loop rather than
/// inside the dialog.
#[test]
fn a_saved_search_reaches_searches_toml() {
    let tree = Tree::new("saved");
    tree.file("keep.rs", b"fn main() {}\n");

    let mut s = Session::start(tree.root(), 120, 30);
    s.wait_for_listing();
    s.open_find();
    s.type_text("*.rs");
    // `Alt+3` selects the Load/Save tab from anywhere in the dialog.
    s.press(b"\x1b[51;3u", "the Load/Save tab", |text| {
        text.contains("Save as")
    });
    // A fresh tab opens on its own first control, so focus is on the saved
    // list: `Load` is one control on and `Save as` is two.
    s.tabs(2);
    s.press(keys::ENTER, "the name prompt", |text| {
        text.contains("Save this search as")
    });
    s.type_text("rust sources");
    s.press(keys::ENTER, "the saved list to gain the name", |text| {
        !text.contains("Save this search as") && text.contains("rust sources")
    });
    // Focus came back on `Save as…`, so `Enter` there would open the prompt
    // again: `Start search` is two controls further on. Starting the search is
    // what commits the list (the dialog changes a list, the event loop writes
    // the file).
    s.tabs(2);
    s.press(keys::ENTER, "the search results", |text| {
        text.contains("keep")
    });

    let path = s.config_home().join("holoscommander").join("searches.toml");
    s.wait_for("searches.toml to be written", |_| path.is_file());
    let text = fs::read_to_string(&path).expect("searches.toml");
    assert!(
        text.contains("rust sources") && text.contains("*.rs"),
        "the saved search is not in the file:\n{text}"
    );
}
