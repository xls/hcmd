//! Handing a file to something outside this program.
//!
//! A file's kind is decided from its **content first and its extension
//! second**: the head is sniffed and only then is the association consulted,
//! so a script without a suffix and a `.txt` full of ELF are both treated as
//! what they are rather than as what they are called.
//!
//! The terminal is given back for exactly the duration of whatever is handed
//! the file and no longer. Nothing here is reached from
//! [`crate::input::dispatch`]: opening reads the file, so `Enter` queues an
//! [`OpenRequest`] and the event loop resolves it, which is also why
//! [`App::service_open`] is the one method in this tree that takes a
//! [`crate::term::Term`].

use crate::app::{App, OpenIntent, OpenRequest, ViewRequest};
use crate::vfs::VfsPath;

impl App {
    /// Queue the open. The event loop resolves the association.
    pub fn request_open(&mut self, request: OpenRequest) {
        self.handoff.open = Some(request);
    }

    /// Whether an open is waiting for the event loop, so `drain_input` stops
    /// on the frame `Enter` queued one.
    pub const fn open_pending(&self) -> bool {
        self.handoff.open.is_some()
    }

    /// Resolve and act on the queued open: sniff the head, apply the execute
    /// policy, and either prompt, run, hand to the desktop or open the viewer.
    ///
    /// The five steps of the design, in order. Reads the
    /// file, so it is the event loop's; `term` is here because
    /// `execute_in = "console"` may put the screen in front of the shell.
    pub fn service_open(&mut self, term: &mut crate::term::Term) -> crate::Result<()> {
        let Some(request) = self.handoff.open.take() else {
            return Ok(());
        };
        let OpenRequest {
            path,
            never_execute,
            intent,
        } = request;
        let name = path.file_name().unwrap_or_default();
        match intent {
            OpenIntent::Resolve => {}
            // the prompt was answered. The head is read again
            // here rather than carried through the dialog, because the dialog
            // holds only what it draws and one extra read of 512 bytes is
            // cheaper than a second copy of the state.
            OpenIntent::Execute => {
                let head = match self.head_of(&path) {
                    Ok((head, _)) => head,
                    Err(err) => {
                        self.message = Some(format!("{name}: {err}"));
                        return Ok(());
                    }
                };
                return self.execute_file(&path, &head, term);
            }
            OpenIntent::Chooser => {
                self.open_with_chooser(&path);
                return Ok(());
            }
            OpenIntent::Application(id) => {
                self.open_with(&path, &id);
                return Ok(());
            }
        }

        // Step 2. One read, and everything below is answered from it: the
        // MIME, the word the prompt puts in front of the user, and whether
        // there is a shebang.
        let (head, mode) = match self.head_of(&path) {
            Ok(both) => both,
            Err(err) => {
                self.message = Some(format!("{name}: {err}"));
                return Ok(());
            }
        };

        let executable = crate::ops::open::is_executable(mode);

        // A container whose *name* claimed nothing. `Enter` guesses by name in
        // `dispatch`, because `dispatch` may not read; the head is in hand
        // here, one frame later, so the guess can be corrected before the file
        // is handed to an application that would only offer to unpack it. An
        // `.apkm` is a zip, and so are a great many other extensions nobody
        // will ever finish tabulating - which is the argument for asking the
        // bytes rather than growing the table.
        //
        // An executable is left alone: a self-extracting archive is both, and
        // `Enter` on it has always meant the execute policy.
        //
        // Only when the cursor is still on the file the request was about. It
        // may have moved while this was queued, and entering an archive the
        // user is no longer pointing at is worse than not entering one.
        if self.config.archive.enter_on_click
            && !executable
            && crate::app::container_kind(&name).is_none()
            && crate::vfs::archive::format::head_is_container(&head)
        {
            let side = self.active_side;
            let still_there = self
                .panel(side)
                .active_tab()
                .current_path()
                .is_some_and(|at| at == path);
            if still_there {
                self.enter_container_under_cursor(side, crate::vfs::BackendKind::Archive);
                return Ok(());
            }
        }
        // Steps 3 and 4. `never_execute` is `Shift+Enter`, which the design
        // says "**always** opens with the associated application, never
        // executes", and `execute = "never"` is the same answer as a setting -
        // both skip straight to the association, so `Enter` "can never launch
        // anything".
        if executable && !never_execute {
            match self.config.open.execute {
                crate::config::ExecutePolicy::Ask => {
                    // Step 1's refusal belongs here and not earlier: a
                    // non-local file is never executed, but it is still
                    // perfectly openable, so only the branch that would run it
                    // refuses.
                    if path.local_path().is_none() {
                        self.message = Some(format!("{name}: {}", crate::ops::open::NOT_LOCAL));
                    } else {
                        let kind = crate::ops::open::kind_of(&head, &name);
                        let size = self.vfs.stat(&path).map(|e| e.size).unwrap_or(0);
                        self.handoff.subject = Some(path);
                        self.push_dialog(Box::new(crate::ui::dialog::ExecuteDialog::new(
                            name,
                            size,
                            kind,
                            &self.config.panel,
                        )));
                    }
                    return Ok(());
                }
                crate::config::ExecutePolicy::Always => {
                    self.execute_file(&path, &head, term)?;
                    return Ok(());
                }
                crate::config::ExecutePolicy::Never => {}
            }
        }

        // Step 5, and only for `Shift+Enter`. `Enter` runs an executable or it
        // explains itself: handing an unrecognised file to the desktop opener
        // is how pressing `Enter` on an `.iso` while walking a directory
        // opened a web browser on a `file://` URL. In a terminal, where
        // `Enter` is how you move around, launching a GUI program is never
        // what it should mean.
        if !never_execute {
            self.message = Some(format!(
                "{name} is not executable - Shift+Enter opens it, F3 views it"
            ));
            return Ok(());
        }
        self.act_on_association(&path, &head, mode);
        Ok(())
    }

    /// The first [`crate::ops::open::HEAD_WINDOW`] bytes of a file, and its
    /// mode bits.
    ///
    /// One read for both questions, because the design both
    /// answer from content and reading twice would let the two disagree.
    fn head_of(&self, path: &VfsPath) -> crate::Result<(Vec<u8>, u32)> {
        use std::io::Read;
        let entry = self.vfs.stat(path)?;
        let mut reader = self.vfs.open_read(path)?;
        let mut head = vec![0_u8; crate::ops::open::HEAD_WINDOW];
        let mut filled = 0;
        while filled < head.len() {
            let Some(slice) = head.get_mut(filled..) else {
                break;
            };
            match reader.read(slice) {
                Ok(0) => break,
                Ok(n) => filled = filled.saturating_add(n),
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
                Err(err) => return Err(err.into()),
            }
        }
        head.truncate(filled);
        Ok((head, entry.mode))
    }

    /// the three-step answer, acted on.
    ///
    /// A handler is spawned, the desktop is handed the file, and anything else
    /// opens in the internal viewer - "so `Enter` on an unknown type shows the
    /// file rather than doing nothing".
    fn act_on_association(&mut self, path: &VfsPath, head: &[u8], mode: u32) {
        let name = path.file_name().unwrap_or_default();
        match crate::ops::open::resolve(&self.config.open, path, head, mode) {
            crate::ops::open::Association::Handler(command) => {
                // An empty command is inert rather than a launch of the file
                // itself: `{file}` is mandatory in a handler template, and a
                // handler with no program would make the file `argv[0]`, which
                // is `Enter` executing something no rule said to execute.
                if command.program.is_empty() {
                    self.message = Some(format!(
                        "{name}: this handler has no command; nothing to run"
                    ));
                    return;
                }
                self.handoff.external = Some(command);
            }
            crate::ops::open::Association::Desktop(local) => {
                if let Err(err) = crate::ops::open::desktop_open(&local) {
                    self.message = Some(format!("{name}: {err}"));
                }
            }
            crate::ops::open::Association::Viewer(target) => {
                self.request_view(ViewRequest::File {
                    path: target,
                    at: None,
                });
            }
        }
    }

    /// the "run it", in whichever of the two places
    /// `open.execute_in` names.
    ///
    /// `console` writes the quoted command line to the PTY and lets the
    /// `switch_on_run` decide whether the screen follows, so output is
    /// visible, stdin works and a TUI program gets a real terminal. `detached`
    /// forks with null stdio and does not wait.
    fn execute_file(
        &mut self,
        path: &VfsPath,
        head: &[u8],
        term: &mut crate::term::Term,
    ) -> crate::Result<()> {
        let name = path.file_name().unwrap_or_default();
        let shell = std::env::var_os("SHELL");
        let argv = match crate::ops::open::execute_argv(path, head, shell.as_deref()) {
            Ok(argv) => argv,
            Err(why) => {
                self.message = Some(format!("{name}: {why}"));
                return Ok(());
            }
        };
        match self.config.open.execute_in {
            crate::config::ExecuteIn::Console => {
                if self.console.shell.is_none() {
                    // No PTY means no console to run in. Say so rather than
                    // silently falling back to `detached`, which is a
                    // different setting with different consequences for
                    // output and stdin.
                    self.message = Some(format!(
                        "{name}: open.execute_in is \"console\" and no shell is running; \
                         start one with Ctrl+O"
                    ));
                    return Ok(());
                }
                let mut line = argv
                    .iter()
                    .map(|word| crate::input::cmdline::shell_quote(word))
                    .collect::<Vec<String>>()
                    .join(" ");
                line.push('\n');
                self.to_shell_internal(line.as_bytes());
                self.command_was_run();
                Ok(())
            }
            crate::config::ExecuteIn::Detached => {
                let parent = path.parent();
                let cwd = parent.as_ref().and_then(VfsPath::local_path);
                if let Err(err) = crate::ops::open::spawn_detached(&argv, cwd) {
                    self.message = Some(format!("{name}: {err}"));
                }
                // `term` is untouched on this path: a detached program has no
                // terminal of ours to hand over. It is a parameter because the
                // console arm's sibling needs one, and taking it here keeps
                // one signature for both.
                let _ = term;
                Ok(())
            }
        }
    }

    /// The file the prompt is asking about, while it is on
    /// screen.
    ///
    /// The dialog carries only what it draws - a name, a size and a word for
    /// the type - because [`crate::dialog::Dialog::handle_key`] is given a key
    /// and nothing else. The path it was built from
    /// waits here for whichever of the four buttons comes back.
    pub fn take_open_subject(&mut self) -> Option<VfsPath> {
        self.handoff.subject.take()
    }

    /// the chooser, opened over the file the prompt was about.
    ///
    /// Reads the desktop entry directories, so it is the event loop's and not
    /// `dispatch`'s.
    pub fn open_with_chooser(&mut self, path: &VfsPath) {
        let name = path.file_name().unwrap_or_default();
        let head = self.head_of(path).map(|(head, _)| head).unwrap_or_default();
        let mime = crate::ops::open::mime_of(&name, &head);
        let apps = crate::ops::open::applications_for(&mime);
        self.handoff.subject = Some(path.clone());
        self.push_dialog(Box::new(crate::ui::dialog::OpenWithDialog::new(name, apps)));
    }

    /// Launch the application the chooser returned.
    pub fn open_with(&mut self, path: &VfsPath, app_id: &str) {
        let name = path.file_name().unwrap_or_default();
        let Some(local) = path.local_path() else {
            self.message = Some(format!("{name}: {}", crate::ops::open::NOT_LOCAL));
            return;
        };
        let head = self.head_of(path).map(|(head, _)| head).unwrap_or_default();
        let mime = crate::ops::open::mime_of(&name, &head);
        let Some(app) = crate::ops::open::applications_for(&mime)
            .into_iter()
            .find(|a| a.id == app_id)
        else {
            self.message = Some(format!("{app_id}: no longer advertised for {mime}"));
            return;
        };
        let argv = crate::ops::open::open_with_argv(&app, local);
        let parent = path.parent();
        let cwd = parent.as_ref().and_then(VfsPath::local_path);
        if let Err(err) = crate::ops::open::spawn_detached(&argv, cwd) {
            self.message = Some(format!("{name}: {err}"));
        }
    }
}
