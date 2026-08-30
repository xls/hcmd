//! the design - the shell's directory and the active panel's, kept in step.
//!
//! > The shell's cwd and the active panel's directory must track each other.
//! >
//! > * **Panel → shell**: when the active panel changes directory, write
//! >   `cd <path>` to the PTY, quoted, only when the shell is at a prompt and
//! >   its input line is empty.
//! > * **Shell → panel**: rely on **OSC 7** … Parse `OSC 7` out of the PTY
//! >   stream and update the active panel. Do not attempt to guess the cwd by
//! >   reading `/proc/<pid>/cwd` of the shell - that races with subprocesses.
//!
//! Both halves are *decisions*, and this module is where they are taken. It
//! holds no `App`, does no I/O beyond one `stat`/`opendir` of the directory the
//! shell named, and writes nothing: [`CwdSync::panel_moved`] returns the bytes
//! for the caller to queue and [`CwdSync::shell_reported`] returns the move for
//! the caller to make. That is what makes every rule below assertable without a
//! PTY, a terminal or a filesystem.
//!
//! # The loop that must not form
//!
//! The two halves feed each other. A panel change writes a `cd`; the `cd`
//! produces a prompt; the prompt emits an `OSC 7` naming the directory we just
//! asked for. Treated as news, that `OSC 7` would move the panel - and by the
//! time it lands the user may have walked on, so it would drag them *back*.
//! Walking into a directory and straight out again is enough to see it.
//!
//! **Comparing paths is not enough**, because the echo is late: when the second
//! `cd` has already been written, the first one's `OSC 7` still names somewhere
//! the panel is not, and it is still not news. So the `cd`s written and not yet
//! answered are **counted** ([`CwdSync::unanswered`]): every `cd` produces
//! exactly one prompt, an `OSC 7` arriving while the count is non-zero is our
//! own echo and is consumed, and only one arriving at zero is the user's own
//! `cd` typed into the shell.
//!
//! # Declining is a feature
//!
//! Most of the outcomes on each side do nothing at all, and each of them is a
//! rule rather than an oversight:
//!
//! * [`Cd::Remote`] - the shell's last `OSC 7` named another host, so it is
//!   inside `ssh` (or anything else that puts a shell from another machine on
//!   the far end of this PTY). A local path means nothing there, and `cd`ing a
//!   remote shell into the local panel's directory is a change nobody asked
//!   for. It resumes the moment a local `OSC 7` arrives.
//! * [`Cd::Unknown`] - the shell does not say where its input line begins
//!   (no `OSC 133 ; B`: `dash`, `fish` without the snippet, a user who set
//!   `console.inject_hooks = false`). "Cannot tell" is not "the line is empty",
//!   and a `cd` written over a half-typed command corrupts it. The panel → shell
//!   half simply does not run, **silently**: a shell that never marks its prompt
//!   would otherwise produce a complaint at every prompt for as long as the
//!   session lasts.
//! * [`Cd::AlreadyThere`] / [`Follow::AlreadyThere`] - the shell is already
//!   where the panel is, or the panel is already where the shell is. Neither is
//!   a change, so neither is a `cd` and neither is a re-read: the design's
//!   directory read is not free, and one per prompt would be a spinner that
//!   never stops.
//! * [`Follow::Unreadable`] - the shell is somewhere the panel cannot list. It
//!   says so and stays where it is; navigating would blank the panel and then
//!   report the failure, which is a hole where a directory used to be. A shell
//!   only needs `+x` on a directory to sit in it, and a panel needs `+r` to list
//!   it, so this is an ordinary Tuesday and not a corner case.

use std::path::{Path, PathBuf};

/// What the panel → shell half decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cd {
    /// Write these bytes to the PTY. A complete line, carriage return included.
    Write(Vec<u8>),
    /// The shell is already in that directory, or already on its way there.
    AlreadyThere,
    /// The shell's input line has something on it. the design allows the `cd`
    /// "only when the shell is at a prompt and its input line is empty", and
    /// this is the "not empty" half.
    Busy,
    /// The shell does not say whether its input line is empty, so nothing is
    /// written and nothing is reported. See the module docs.
    Unknown,
    /// The shell is **not on this machine**: its last `OSC 7` named another
    /// host (the authority check), which is what `ssh` in the
    /// console looks like from here.
    ///
    /// A local path means nothing on the other end. Writing `cd '/etc'` into an
    /// `ssh` session moves the *remote* shell into the remote `/etc` - a
    /// directory change nobody asked for, on a machine the panel is not
    /// showing. So nothing is written until a local `OSC 7` says the shell is
    /// back.
    Remote,
}

impl Cd {
    /// The bytes to write, when there are any.
    pub fn bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Write(bytes) => Some(bytes),
            Self::AlreadyThere | Self::Busy | Self::Unknown | Self::Remote => None,
        }
    }
}

/// What the shell → panel half decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Follow {
    /// Move the active panel to this directory.
    Navigate(PathBuf),
    /// The echo of a `cd` this application wrote. Consumed; see the module docs.
    Echo,
    /// The active panel is already showing that directory.
    AlreadyThere,
    /// The active panel is not a local listing - an archive or a
    /// remote host has its own idea of where it is, and the local
    /// shell does not get to move it.
    Foreign,
    /// The directory the shell named cannot be listed. The panel stays where it
    /// is and [`Follow::message`] says why.
    Unreadable {
        /// The directory the shell reported.
        path: PathBuf,
        /// Why it could not be listed, as a sentence fragment: "does not
        /// exist", "cannot be read: permission denied".
        why: String,
    },
}

impl Follow {
    /// The one line for the status bar, when there is something to say.
    ///
    /// Only [`Follow::Unreadable`] has anything: the other four outcomes are
    /// either the panel moving (which is its own feedback) or a deliberate
    /// silence.
    pub fn message(&self) -> Option<String> {
        match self {
            Self::Unreadable { path, why } => Some(format!(
                "the shell moved to {}, which {why}",
                path.display()
            )),
            Self::Navigate(_) | Self::Echo | Self::AlreadyThere | Self::Foreign => None,
        }
    }

    /// Where the panel should go, when it should go anywhere.
    pub fn navigate_to(&self) -> Option<&Path> {
        match self {
            Self::Navigate(path) => Some(path),
            Self::Echo | Self::AlreadyThere | Self::Foreign | Self::Unreadable { .. } => None,
        }
    }
}

/// The state the design needs, and all of it.
///
/// One per shell. **Reset it with the shell** - a new `Console` knows nothing
/// about where the old one was, and an inherited count would have the first
/// `cd` of the new session swallowed as the echo of a write to a PTY that no
/// longer exists. `crate::app::App::set_console` is where that happens.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CwdSync {
    /// Where the shell is, or is on its way to.
    shell_cwd: Option<PathBuf>,
    /// `cd`s written and not yet answered by a prompt.
    unanswered: usize,
    /// The last `OSC 7` named a host that is not this machine, so the shell on
    /// the other end of the PTY is somewhere else - `ssh`, `docker exec`, a
    /// serial console. See [`Cd::Remote`].
    remote: bool,
}

impl CwdSync {
    /// A synchroniser for a shell that has not said anything yet.
    pub const fn new() -> Self {
        Self {
            shell_cwd: None,
            unanswered: 0,
            remote: false,
        }
    }

    /// Forget everything: a different shell is on the other end now.
    pub fn forget(&mut self) {
        *self = Self::new();
    }

    /// Where the shell last said it was, or where it was last told to go.
    pub fn shell_cwd(&self) -> Option<&Path> {
        self.shell_cwd.as_deref()
    }

    /// How many written `cd`s are still waiting for their prompt.
    pub const fn unanswered(&self) -> usize {
        self.unanswered
    }

    /// Whether the shell last said it was on another machine.
    pub const fn is_remote(&self) -> bool {
        self.remote
    }

    /// **Shell → nothing.** An `OSC 7` arrived naming another host.
    ///
    /// the design ignores it for moving the panel - "a hostname that is not
    /// this machine" is not this shell's directory - but it is not nothing:
    ///
    /// * The panel → shell half stops until a local `OSC 7` arrives. A `cd`
    ///   written into an `ssh` session moves a shell on another machine.
    /// * It still **answers** an outstanding `cd`. The count exists to swallow
    ///   the echo of our own writes, and a rejected sequence that is never
    ///   counted leaks one for the length of the remote session - after which
    ///   the first genuine local `cd` is swallowed as an echo and the panel
    ///   stops following the shell.
    /// * Where the shell is becomes unknown, so no local path may be compared
    ///   against it.
    pub fn shell_is_foreign(&mut self) {
        self.remote = true;
        self.shell_cwd = None;
        self.unanswered = self.unanswered.saturating_sub(1);
    }

    /// **Panel → shell.** The active panel moved to `target`; decide what the
    /// shell should be told.
    ///
    /// `input_is_empty` is [`crate::console::Console::input_is_empty`], and its
    /// `None` is load-bearing: it means the shell does not mark where its input
    /// begins, which is *not* the same as an empty line. Only `Some(true)`
    /// writes anything.
    ///
    /// The caller has already decided that this is the active panel, that the
    /// console is alive, that `console.sync_cwd` is on and that the panel is a
    /// local directory; none of those are this module's to know.
    pub fn panel_moved(&mut self, target: &Path, input_is_empty: Option<bool>) -> Cd {
        // The shell is on another machine. Checked first: everything below it
        // reasons about a local path, and none of that reasoning applies to a
        // shell that is somewhere else.
        if self.remote {
            return Cd::Remote;
        }
        // Already there, or already on the way there. Checked before the input
        // line, because a shell that cannot say whether its line is empty
        // should still not be sent somewhere it already is.
        if self.shell_cwd.as_deref() == Some(target) {
            return Cd::AlreadyThere;
        }
        match input_is_empty {
            None => Cd::Unknown,
            Some(false) => Cd::Busy,
            Some(true) => {
                let line = cd_line(target);
                self.shell_cwd = Some(target.to_path_buf());
                self.unanswered = self.unanswered.saturating_add(1);
                Cd::Write(line)
            }
        }
    }

    /// **Shell → panel.** An `OSC 7` arrived; decide what the panel should do.
    ///
    ///
    /// `panel_at` is the active panel's directory, or `None` when it is not a
    /// local listing at all.
    ///
    /// Touches the filesystem exactly once, and only for a directory change
    /// that is genuinely news: [`readable`] answers whether the panel could
    /// list it before the panel is emptied to try.
    pub fn shell_reported(&mut self, cwd: PathBuf, panel_at: Option<&Path>) -> Follow {
        self.shell_reported_with(cwd, panel_at, &readable)
    }

    /// [`CwdSync::shell_reported`] with the readability probe supplied.
    ///
    /// The seam the tests drive: every rule here can then be asserted against a
    /// directory that never existed, on a machine where the test suite may not
    /// create one.
    pub fn shell_reported_with(
        &mut self,
        cwd: PathBuf,
        panel_at: Option<&Path>,
        probe: &dyn Fn(&Path) -> Result<(), String>,
    ) -> Follow {
        // A local `OSC 7`: the shell is back on this machine, whatever it was
        // doing before (the authority check is the only thing that
        // can say so).
        self.remote = false;
        // Our own `cd` coming back. See the module docs: counted, not compared.
        if self.unanswered > 0 {
            self.unanswered = self.unanswered.saturating_sub(1);
            self.shell_cwd = Some(cwd);
            return Follow::Echo;
        }
        // The shell is the authority on where the shell is, so this is recorded
        // whatever the panel then does - including when the panel declines to
        // follow, so that a later panel move still writes its `cd`.
        self.shell_cwd = Some(cwd.clone());

        let Some(panel_at) = panel_at else {
            return Follow::Foreign;
        };
        if panel_at == cwd {
            return Follow::AlreadyThere;
        }
        match probe(&cwd) {
            Ok(()) => Follow::Navigate(cwd),
            Err(why) => Follow::Unreadable { path: cwd, why },
        }
    }
}

/// The line that moves the shell: `cd <quoted path>`, with the carriage return
/// that runs it (the "write `cd <path>` to the PTY, quoted").
///
/// Bytes rather than a `String`, because a directory name on this filesystem is
/// bytes: a path that is not UTF-8 would come out of a lossy conversion as a
/// different directory, and `cd`ing to a *different* directory is worse than
/// not `cd`ing at all.
///
/// `\r`, not `\n`: the PTY is in canonical mode and the shell's line editor
/// reads the carriage return the Return key would have produced.
pub fn cd_line(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt as _;

    let mut line = b"cd ".to_vec();
    line.extend_from_slice(&quote(path.as_os_str().as_bytes()));
    line.push(b'\r');
    line
}

/// Quote a path for a POSIX shell, over bytes.
///
/// The same rule as [`crate::input::cmdline::shell_quote`], which is the `str` half of
/// it for the command line - `quoting_agrees_with_the_command_line` below pins the two
/// together. This one exists because a `cd` is written from a `Path`, and a `Path` is
/// not necessarily UTF-8.
///
/// Single quotes protect every byte except a single quote itself, which is
/// closed, escaped and reopened.
pub fn quote(raw: &[u8]) -> Vec<u8> {
    let safe = |b: u8| {
        b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'.' | b'_' | b'-' | b'/' | b'@' | b'+' | b',' | b'=' | b':' | b'%'
            )
    };
    if !raw.is_empty() && raw.iter().all(|b| safe(*b)) && raw.first() != Some(&b'-') {
        return raw.to_vec();
    }
    let mut out = Vec::with_capacity(raw.len().saturating_add(2));
    out.push(b'\'');
    for b in raw {
        if *b == b'\'' {
            out.extend_from_slice(b"'\\''");
        } else {
            out.push(*b);
        }
    }
    out.push(b'\'');
    out
}

/// Could the panel list this directory?
///
/// The question the design does not ask and the panel cannot do without: the
/// shell reports where it is, and where it is may be a directory that has been
/// deleted underneath it, a path that is a file, or a directory it can sit in
/// (`+x`) but that we cannot read (`-r`). `crate::app::App::navigate` empties
/// the tab before the listing starts, so a panel sent there shows nothing at
/// all and then an error - a hole where a directory used to be. This is asked
/// first instead, and the panel is left alone.
///
/// `Err` carries the reason as a sentence fragment, for [`Follow::message`].
///
/// It is two syscalls on a local path, on the frame loop, and only for an
/// `OSC 7` that is genuinely a directory change - at most one per `cd` the user
/// types. The asynchronous `crate::vfs` read is not an option here: its answer
/// arrives after the panel has already been emptied to receive it.
pub fn readable(path: &Path) -> Result<(), String> {
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => return Err("is not a directory".to_string()),
        Err(err) => return Err(describe(&err)),
    }
    // `metadata` follows the symlink and says "directory"; only opening it says
    // whether this process may read it.
    match std::fs::read_dir(path) {
        Ok(_) => Ok(()),
        Err(err) => Err(describe(&err)),
    }
}

/// An `io::Error` as the fragment [`Follow::message`] puts after "which".
fn describe(err: &std::io::Error) -> String {
    match err.kind() {
        std::io::ErrorKind::NotFound => "does not exist".to_string(),
        std::io::ErrorKind::PermissionDenied => "cannot be read: permission denied".to_string(),
        std::io::ErrorKind::NotADirectory => "is not a directory".to_string(),
        _ => format!("cannot be read: {err}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A probe that says yes to everything, for the rules that are not about
    /// the filesystem.
    fn anything(_: &Path) -> Result<(), String> {
        Ok(())
    }

    /// A probe that says no to everything.
    fn nothing(_: &Path) -> Result<(), String> {
        Err("does not exist".to_string())
    }

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    // ------------------------------------------------- panel -> shell ------

    #[test]
    fn a_panel_move_writes_a_quoted_cd_when_the_line_is_empty() {
        let mut sync = CwdSync::new();
        assert_eq!(
            sync.panel_moved(Path::new("/usr/share"), Some(true)),
            Cd::Write(b"cd /usr/share\r".to_vec())
        );
        assert_eq!(sync.shell_cwd(), Some(Path::new("/usr/share")));
        assert_eq!(sync.unanswered(), 1, "and it is waiting for its prompt");
    }

    #[test]
    fn a_path_that_needs_quoting_gets_it() {
        let mut sync = CwdSync::new();
        let cd = sync.panel_moved(Path::new("/tmp/My Reports (final)"), Some(true));
        assert_eq!(cd, Cd::Write(b"cd '/tmp/My Reports (final)'\r".to_vec()));

        let mut sync = CwdSync::new();
        let cd = sync.panel_moved(Path::new("/tmp/it's here"), Some(true));
        assert_eq!(cd, Cd::Write(b"cd '/tmp/it'\\''s here'\r".to_vec()));
    }

    #[test]
    fn a_path_that_is_not_utf_8_is_still_the_right_directory() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt as _;

        let raw = OsStr::from_bytes(b"/tmp/\xff\xfe-broken");
        let mut sync = CwdSync::new();
        let cd = sync.panel_moved(Path::new(raw), Some(true));
        // Quoted whole, byte for byte - a lossy conversion would have written
        // U+FFFD and `cd`ed somewhere that does not exist.
        assert_eq!(cd, Cd::Write(b"cd '/tmp/\xff\xfe-broken'\r".to_vec()));
    }

    #[test]
    fn nothing_is_written_at_a_line_this_application_cannot_read() {
        // the "only when …". `None` is not `false`.
        let mut sync = CwdSync::new();
        assert_eq!(sync.panel_moved(Path::new("/usr"), None), Cd::Unknown);
        assert_eq!(sync.shell_cwd(), None, "and nothing was assumed about it");
        assert_eq!(sync.unanswered(), 0);
    }

    #[test]
    fn nothing_is_written_over_a_half_typed_command() {
        let mut sync = CwdSync::new();
        assert_eq!(sync.panel_moved(Path::new("/usr"), Some(false)), Cd::Busy);
        assert_eq!(sync.unanswered(), 0);
    }

    #[test]
    fn a_directory_the_shell_is_already_in_is_not_re_sent() {
        let mut sync = CwdSync::new();
        assert!(matches!(
            sync.panel_moved(Path::new("/usr"), Some(true)),
            Cd::Write(_)
        ));
        assert_eq!(
            sync.panel_moved(Path::new("/usr"), Some(true)),
            Cd::AlreadyThere,
            "a second move to the same place says nothing twice"
        );
        assert_eq!(sync.unanswered(), 1, "and does not expect a second prompt");
    }

    #[test]
    fn a_shell_reporting_where_it_is_stops_the_panel_telling_it_again() {
        let mut sync = CwdSync::new();
        assert!(matches!(
            sync.shell_reported_with(p("/etc"), Some(Path::new("/usr")), &anything),
            Follow::Navigate(_)
        ));
        assert_eq!(
            sync.panel_moved(Path::new("/etc"), Some(true)),
            Cd::AlreadyThere,
            "the panel following the shell must not send the shell after it"
        );
    }

    // ------------------------------------------------- shell -> panel ------

    #[test]
    fn an_osc_7_at_rest_moves_the_panel() {
        let mut sync = CwdSync::new();
        assert_eq!(
            sync.shell_reported_with(p("/var/log"), Some(Path::new("/usr")), &anything),
            Follow::Navigate(p("/var/log"))
        );
        assert_eq!(sync.shell_cwd(), Some(Path::new("/var/log")));
    }

    #[test]
    fn an_osc_7_naming_where_the_panel_already_is_costs_nothing() {
        // A prompt emits one per prompt; a re-read per prompt would be a
        // directory listing that never stops.
        let mut sync = CwdSync::new();
        assert_eq!(
            sync.shell_reported_with(p("/usr"), Some(Path::new("/usr")), &anything),
            Follow::AlreadyThere
        );
    }

    #[test]
    fn the_echo_of_our_own_cd_is_consumed() {
        let mut sync = CwdSync::new();
        let _ = sync.panel_moved(Path::new("/usr/share"), Some(true));
        assert_eq!(
            sync.shell_reported_with(p("/usr/share"), Some(Path::new("/usr/share")), &anything),
            Follow::Echo
        );
        assert_eq!(sync.unanswered(), 0, "and the account is settled");
    }

    #[test]
    fn walking_in_and_straight_back_out_does_not_bounce() {
        // The regression the counter exists for. Two `cd`s are written before
        // either prompt answers; both echoes must be consumed, even though the
        // first names a directory the panel has already left.
        let mut sync = CwdSync::new();
        let _ = sync.panel_moved(Path::new("/usr/share"), Some(true));
        let _ = sync.panel_moved(Path::new("/usr"), Some(true));
        assert_eq!(sync.unanswered(), 2);

        assert_eq!(
            sync.shell_reported_with(p("/usr/share"), Some(Path::new("/usr")), &anything),
            Follow::Echo,
            "comparing paths would have called this news and dragged the panel back"
        );
        assert_eq!(
            sync.shell_reported_with(p("/usr"), Some(Path::new("/usr")), &anything),
            Follow::Echo
        );
        assert_eq!(sync.unanswered(), 0);
    }

    #[test]
    fn a_users_own_cd_after_the_echoes_is_still_news() {
        let mut sync = CwdSync::new();
        let _ = sync.panel_moved(Path::new("/usr/share"), Some(true));
        assert_eq!(
            sync.shell_reported_with(p("/usr/share"), Some(Path::new("/usr/share")), &anything),
            Follow::Echo
        );
        assert_eq!(
            sync.shell_reported_with(p("/etc"), Some(Path::new("/usr/share")), &anything),
            Follow::Navigate(p("/etc")),
            "the count is back at zero, so this one is the user's"
        );
    }

    #[test]
    fn a_shell_on_another_machine_is_not_sent_local_directories() {
        // the authority check, applied to the panel → shell half:
        // inside `ssh` the shell on the far end of the pty is somewhere else,
        // and `cd '/etc'` written into it moves a *remote* shell into a remote
        // directory nobody asked for. The remote prompt marks itself - starship,
        // oh-my-zsh and fish 4 all emit `OSC 133` - so "the input line is empty"
        // is answered about the remote prompt and the write would go ahead.
        let mut sync = CwdSync::new();
        let _ = sync.panel_moved(Path::new("/usr/share"), Some(true));
        assert_eq!(sync.unanswered(), 1);

        sync.shell_is_foreign();
        assert!(sync.is_remote());
        assert_eq!(
            sync.unanswered(),
            0,
            "the remote prompt still answered our `cd`; a leaked count would \
             swallow the first genuine local one for the rest of the session"
        );
        assert_eq!(sync.shell_cwd(), None, "where it is, is not knowable");

        for target in ["/etc", "/usr", "/var", "/opt"] {
            assert_eq!(
                sync.panel_moved(Path::new(target), Some(true)),
                Cd::Remote,
                "nothing is written while the shell is on another machine"
            );
        }
        assert_eq!(sync.unanswered(), 0, "and nothing is waiting for a prompt");

        // `exit`: a local `OSC 7` says the shell is back, and it is news.
        assert_eq!(
            sync.shell_reported_with(p("/srv"), Some(Path::new("/usr")), &anything),
            Follow::Navigate(p("/srv"))
        );
        assert!(!sync.is_remote());
        assert_eq!(
            sync.panel_moved(Path::new("/etc"), Some(true)),
            Cd::Write(b"cd /etc\r".to_vec()),
            "and the panel → shell half works again"
        );
    }

    #[test]
    fn a_new_shell_starts_the_bookkeeping_again() {
        let mut sync = CwdSync::new();
        let _ = sync.panel_moved(Path::new("/usr/share"), Some(true));
        sync.forget();
        assert_eq!(sync.unanswered(), 0);
        assert_eq!(sync.shell_cwd(), None);
        assert_eq!(
            sync.shell_reported_with(p("/usr/share"), Some(Path::new("/usr")), &anything),
            Follow::Navigate(p("/usr/share")),
            "an inherited count would have swallowed the new shell's first word"
        );
    }

    #[test]
    fn a_panel_that_is_not_a_local_directory_is_not_the_shells_to_move() {
        // An archive or a remote host.
        let mut sync = CwdSync::new();
        assert_eq!(
            sync.shell_reported_with(p("/etc"), None, &anything),
            Follow::Foreign
        );
        assert_eq!(
            sync.shell_cwd(),
            Some(Path::new("/etc")),
            "the shell is still where it says it is"
        );
    }

    #[test]
    fn a_directory_that_cannot_be_listed_leaves_the_panel_alone_and_says_so() {
        let mut sync = CwdSync::new();
        let follow = sync.shell_reported_with(p("/gone"), Some(Path::new("/usr")), &nothing);
        assert_eq!(
            follow,
            Follow::Unreadable {
                path: p("/gone"),
                why: "does not exist".to_string()
            }
        );
        assert_eq!(follow.navigate_to(), None, "the panel does not move");
        assert_eq!(
            follow.message().unwrap_or_default(),
            "the shell moved to /gone, which does not exist",
            "and the user is told rather than left looking at a hole"
        );
    }

    #[test]
    fn a_directory_that_cannot_be_read_does_not_stop_the_next_one_being_followed() {
        let mut sync = CwdSync::new();
        let _ = sync.shell_reported_with(p("/root"), Some(Path::new("/usr")), &nothing);
        assert_eq!(
            sync.shell_reported_with(p("/etc"), Some(Path::new("/usr")), &anything),
            Follow::Navigate(p("/etc"))
        );
    }

    // ------------------------------------------------------- the probe -----

    /// A throwaway directory, removed on drop. Built by hand - `tempfile` is
    /// not on the dependency table (`vfs::local`'s tests do the same).
    struct TempTree {
        root: PathBuf,
    }

    impl TempTree {
        fn new(tag: &str) -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or(std::time::Duration::ZERO)
                .as_nanos();
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "hcmd-sync-{tag}-{pid}-{nanos}-{n}",
                pid = std::process::id()
            ));
            std::fs::create_dir_all(&root).expect("create temp tree");
            Self { root }
        }

        fn path(&self) -> &Path {
            &self.root
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            use std::os::unix::fs::PermissionsExt as _;
            let _ = std::fs::set_permissions(&self.root, std::fs::Permissions::from_mode(0o755));
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn the_probe_answers_for_a_real_directory() {
        let tree = TempTree::new("ok");
        assert_eq!(readable(tree.path()), Ok(()));
    }

    #[test]
    fn the_probe_names_what_is_wrong() {
        let tree = TempTree::new("bad");
        assert_eq!(
            readable(&tree.path().join("never-existed")),
            Err("does not exist".to_string())
        );

        let file = tree.path().join("a-file");
        std::fs::write(&file, "x").expect("write");
        assert_eq!(readable(&file), Err("is not a directory".to_string()));
    }

    #[test]
    fn a_directory_the_shell_may_sit_in_but_we_may_not_list_is_refused() {
        use std::os::unix::fs::PermissionsExt as _;
        if unsafe_to_test_permissions() {
            eprintln!("running as root; skipping the unreadable-directory test");
            return;
        }
        let tree = TempTree::new("perm");
        let dir = tree.path().join("execute-only");
        std::fs::create_dir(&dir).expect("create");
        // 0111: a shell can `cd` into it, a panel cannot list it. This is the
        // case that makes the probe worth having.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o111)).expect("chmod");

        let refused = readable(&dir);
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755));
        assert_eq!(
            refused,
            Err("cannot be read: permission denied".to_string())
        );
    }

    /// `root` ignores the permission bits, so the test above proves nothing
    /// there.
    fn unsafe_to_test_permissions() -> bool {
        use std::os::unix::fs::MetadataExt as _;

        // Asked of a file this process creates, rather than of `/proc/self`,
        // which does not exist on macOS: there the old spelling answered "not
        // root" whoever was running, so as root the test below would have run
        // and failed rather than stepping aside.
        let probe = std::env::temp_dir().join(format!("hcmd-uid-{}", std::process::id()));
        let Ok(()) = std::fs::write(&probe, b"") else {
            return false;
        };
        let root = std::fs::metadata(&probe)
            .map(|m| m.uid() == 0)
            .unwrap_or(false);
        let _ = std::fs::remove_file(&probe);
        root
    }

    // ---------------------------------------------------------- quoting ----

    #[test]
    fn quoting_agrees_with_the_command_line() {
        // One rule, two spellings: `input::cmdline::shell_quote` is the `str`
        // half and `quote` is the byte half. If
        // one changes, this says so.
        for sample in [
            "/usr/share",
            "/tmp/My Reports (final)",
            "/tmp/it's here",
            "/tmp/a;b|c",
            "/tmp/50%",
            "/tmp/a\tb",
            "",
            "-dashed",
            "/tmp/ünïcøde",
        ] {
            assert_eq!(
                String::from_utf8_lossy(&quote(sample.as_bytes())),
                crate::input::cmdline::shell_quote(sample),
                "{sample:?} is quoted two different ways"
            );
        }
    }
}
