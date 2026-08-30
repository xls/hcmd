//! `Enter` on a row: going into whatever it turns out to be.
//!
//! A path segment is pushed **optimistically** when a container is entered.
//! `bundle.zip` is a directory as far as the panel is concerned, and whether
//! it really is one is not known until a backend has tried to read it, so the
//! panel goes there first and the first listing decides. If it proves not to
//! be a container the panel is returned to exactly where it was, which is what
//! [`crate::app::ContainerAttempt`] remembers and what makes `Enter` on a
//! damaged archive a message rather than an empty directory nobody can leave.
//!
//! Nothing here reads a file: [`crate::input::dispatch`] may not, so `Enter`
//! queues an [`crate::app::OpenRequest`] or a read and the event loop resolves
//! it.

use crate::app::{App, ContainerAttempt, OpenRequest, ViewRequest, ViewerStart};
use crate::app::{container_kind, leaving_name};
use crate::ops::{JobKind, JobSpec};
use crate::panel::Side;
use crate::vfs::BackendKind;

impl App {
    /// `Enter` on the entry under the cursor.
    ///
    /// Directories and `..` are handled here. Opening a *file* - associations
    /// and the execute policy - is not built, so it says which milestone brings
    /// it rather than doing nothing.
    ///
    /// **the design does not say which milestone that is**, and this used to
    /// name v0.3 on the reasoning that the `execute_in = "console"` needs the
    /// PTY. The PTY is here and the rest is not: the execute prompt needs
    /// `infer` to say what a file actually is, and the association chain
    /// needs `open`, `mime_guess` and `freedesktop-desktop-entry` - four
    /// crates that v0.3 does not add. What is left is v0.7's "the rest of
    /// TC", which is where every other named but unscheduled feature sits, so
    /// that is what it says. Recorded here because it is inference rather
    /// than something the design states.
    pub fn open_under_cursor(&mut self) {
        if self.enter_directory_under_cursor() {
            return;
        }
        let side = self.active_side;
        let tab = self.active_panel().active_tab();
        let Some(entry) = tab.current() else {
            return;
        };
        let name = entry.name.clone();
        let hit = entry.hit.clone();
        // "For a content match, `Enter` opens the viewer at the
        // matching line with the hit already highlighted - by the same
        // `grep-regex` matcher that found it." Before the archive branch,
        // because a row that carries a hit is a file whose *contents* matched
        // and opening it as a container would answer a question nobody asked.
        // The address is the row's real home, like every other operation on a
        // search result.
        let at_hit = hit.and_then(|hit| tab.current_path().map(|path| (hit, path)));
        let find = tab.virtual_view().and_then(|view| view.find.clone());
        if let Some((hit, path)) = at_hit {
            // No find query means the search was a regex one, which the
            // viewer's find bar cannot compile yet: it opens at the hit and
            // says so rather than silently showing an unhighlighted line.
            self.message = Some(match (hit.line, find.is_some()) {
                (Some(line), true) => format!("{name}: line {line}"),
                (None, true) => format!("{name}: byte {}", hit.offset),
                (Some(line), false) => format!(
                    "{name}: line {line}; opened at the match, and the viewer's regex find is {}",
                    crate::viewer::find::REGEX_MILESTONE
                ),
                (None, false) => format!(
                    "{name}: opened at the match, and the viewer's regex find is {}",
                    crate::viewer::find::REGEX_MILESTONE
                ),
            });
            // the design says "at the matching line", and for three of the
            // four charsets the line number is the only half of the hit that
            // says where that is: the offset counts bytes of the decoded
            // stream. A decoded hit with no line number at all is nothing the
            // searcher produces - `line_number(true)` is set for every search
            // - but if one ever arrives, its own offset is a better answer
            // than the top of the file.
            let start = match (hit.decoded, hit.line) {
                (true, Some(line)) => crate::viewer::HitStart::Line(line),
                (true, None) | (false, _) => crate::viewer::HitStart::Offset(hit.offset),
            };
            self.request_view(ViewRequest::File {
                path,
                at: Some(Box::new(ViewerStart {
                    start,
                    line: hit.line,
                    find,
                })),
            });
            return;
        }
        // the table: "recognized archive -> enter it as a VFS directory", and
        // the design puts a disk image on the same key. *Recognised* is the
        // name here and only the name: both detect by content first, but
        // reading the content is filesystem access and `dispatch` may not do
        // one. The name is what says "this is worth opening as a container";
        // the content decides whether it was, one frame later, and
        // [`App::apply_vfs_event`] puts the panel back if it was not.
        // `Ctrl+PgDn` is the key for a container whose extension says nothing.
        if self.config.archive.enter_on_click
            && let Some(kind) = container_kind(&name)
        {
            self.enter_container_under_cursor(side, kind);
            return;
        }
        // Opening a file means the associated application or the execute
        // policy. Queued rather than done here, because
        // resolving one reads the first window of the file and `dispatch` may
        // not read.
        if let Some(path) = self.active_panel().active_tab().current_path() {
            self.request_open(OpenRequest::new(path, false));
        }
    }

    /// `Shift+Enter`: open with the association, **never** execute.
    ///
    ///
    /// The same request as `Enter` with the one flag that skips the execute
    /// policy entirely, so the key is a promise rather than a preference:
    /// nothing it opens can run.
    pub fn open_with_association(&mut self) {
        if self.enter_directory_under_cursor() {
            return;
        }
        if let Some(path) = self.active_panel().active_tab().current_path() {
            self.request_open(OpenRequest::new(path, true));
        }
    }

    /// `Ctrl+PgDn`: "enter as a directory", **forcing container entry for
    /// containers with odd extensions**.
    ///
    /// The difference from `Enter` is exactly the one the design names: no
    /// extension is consulted and `archive.enter_on_click` does not apply, so
    /// a `backup.dat` that is really a zip opens. What it *is* still comes
    /// from its content, in [`crate::vfs::archive::format::detect`], and a
    /// file that is not a container at all fails the listing and leaves the
    /// panel where it was.
    ///
    /// An archive is tried first, exactly as it always was. What is new is
    /// that an attempt which fails without listing a single row is made
    /// **once** more as a disk image before the panel goes back, which is what
    /// makes the "an extension is a hint and never the answer"
    /// true at the point of entry as well as at the point of detection: a
    /// `backup.dat` that is a disk image opens too. One retry, never two, and
    /// only here. `Enter` decides by name and does not retry, which is the
    /// line the design draws between the two keys.
    pub fn enter_as_dir(&mut self) {
        if self.enter_directory_under_cursor() {
            return;
        }
        let side = self.active_side;
        if let Some(generation) = self.enter_container_under_cursor(side, BackendKind::Archive)
            && let Some(attempt) = self.container_attempts.get_mut(&generation)
        {
            attempt.retry = true;
        }
    }

    /// The half of `Enter` and `Ctrl+PgDn` that is the same: a directory, a
    /// symlink to one, or `..`. True when it handled the entry.
    fn enter_directory_under_cursor(&mut self) -> bool {
        let side = self.active_side;
        let tab = self.active_panel().active_tab();
        let Some(entry) = tab.current() else {
            return true;
        };
        if !(entry.is_dir() || entry.is_parent) {
            return false;
        }
        // `..` is a parent navigation however it was reached, so it lands
        // the cursor on the directory being left exactly as `Backspace` and
        // `Ctrl+PgUp` do. Reaching the same place by a different key must
        // not lose your place in the parent listing - and leaving an archive
        // is that same move, which is why the name comes from
        // [`leaving_name`] rather than from `VfsPath::file_name`.
        let select = entry.is_parent.then(|| leaving_name(&tab.path)).flatten();
        if let Some(path) = tab.current_path() {
            self.navigate_selecting(side, path, select);
        }
        true
    }

    /// Enter the entry under `side`'s cursor as a container of `kind`.
    ///
    ///
    /// Pushes one segment rooted at `/`, so the panel path becomes
    /// `.../foo.zip#/` exactly as the design writes it, or `.../disk.img#/`
    /// as the design does, and remembers where the panel came from until
    /// the listing proves the file really was one.
    ///
    /// Nesting needs nothing extra here: the entry under the cursor inside a
    /// container already has a container path, so a second segment lands on it
    /// and [`crate::vfs::ArchiveSession`] materialises the inner one in the
    /// session cache on the way.
    ///
    /// Returns the generation of the read that will answer the attempt, which
    /// is how [`App::enter_as_dir`] marks its one retry as owed. `None` when
    /// there was nothing under the cursor to enter and no attempt was made:
    /// the current generation belongs to some earlier read then, and marking
    /// that one would retry a container the user never asked about.
    fn enter_container_under_cursor(&mut self, side: Side, kind: BackendKind) -> Option<u64> {
        let tab = self.panel(side).active_tab();
        let entry = tab.current()?;
        if entry.is_parent {
            return None;
        }
        let name = entry.name.clone();
        let from = tab.path.clone();
        let container = tab.current_path()?;
        let inside = container.clone().with_segment(kind, "/");
        self.navigate_selecting(side, inside, None);
        let generation = self.generation;
        let tab_index = self.panel(side).active_index();
        self.container_attempts.insert(
            generation,
            ContainerAttempt {
                side,
                tab: tab_index,
                from,
                name,
                container,
                tried: kind,
                retry: false,
            },
        );
        Some(generation)
    }

    /// `Ctrl+PgDn`'s one retry: enter the same file again as a disk image.
    ///
    ///
    /// Reached only from [`App::apply_vfs_event`], when an archive attempt
    /// that was owed a retry failed without listing a single row. The new
    /// attempt carries the same panel, the same origin and the same name, so a
    /// second failure restores the panel exactly as the first would have, and
    /// it is owed no retry of its own.
    pub(super) fn retry_as_image(&mut self, attempt: ContainerAttempt) {
        let inside = attempt
            .container
            .clone()
            .with_segment(BackendKind::Image, "/");
        self.navigate_selecting(attempt.side, inside, None);
        let generation = self.generation;
        let tab_index = self.panel(attempt.side).active_index();
        self.container_attempts.insert(
            generation,
            ContainerAttempt {
                tab: tab_index,
                tried: BackendKind::Image,
                retry: false,
                ..attempt
            },
        );
    }

    /// `Alt+F6`: "unpacks the archive under the cursor to the other panel's
    /// directory".
    ///
    /// It is an ordinary `F5` with the archive's *root* as its source, which
    /// is the whole point of the trait: extraction is
    /// [`crate::ops`]'s copy engine reading through [`Vfs::open_read`], with
    /// its progress dialog, its conflict handling and its
    /// the design summary, and there is no archive-specific extraction
    /// path to keep in step with it.
    ///
    /// The source is the archive root rather than its members because a
    /// listing of the members is not available without reading the archive,
    /// and `dispatch` may not. A root has no file name, which is precisely
    /// what tells the copy engine to put its *contents* into the destination
    /// rather than a directory named after it - the same rule `cp -r /`
    /// follows.
    pub fn unpack_under_cursor(&mut self) {
        let side = self.active_side;
        let tab = self.active_panel().active_tab();
        let Some(entry) = tab.current() else {
            self.message = Some("there is nothing under the cursor to unpack".to_string());
            return;
        };
        if entry.is_parent || entry.is_dir() {
            self.message = Some(format!(
                "{} is a directory, not an archive; Alt+F5 packs, Alt+F6 unpacks",
                entry.name
            ));
            return;
        }
        let name = entry.name.clone();
        let Some(container) = tab.current_path() else {
            self.message = Some(format!("{name} has no path to unpack"));
            return;
        };
        let dest = self.panel(side.other()).active_tab().path.clone();
        let root = container.with_segment(BackendKind::Archive, "/");
        self.message = Some(format!("unpacking {name} into {dest}"));
        self.request_job(JobSpec::new(JobKind::Copy, vec![root], Some(dest)));
    }
}
