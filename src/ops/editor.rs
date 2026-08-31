//! `F4` and `Shift+F4` - the external editor.
//!
//! There is no internal editor in v1. `F4` hands the terminal to whatever the
//! user already edits with, waits for it, and takes the terminal back.
//!
//! # The sequence, and why every step is here
//!
//! the design spells it out:
//!
//! > leave alternate screen → restore cooked mode → pop keyboard enhancement
//! > flags → spawn with inherited stdio → wait → re-enter alternate screen →
//! > raw mode → push flags → force full redraw → reread the panel.
//!
//! The first three and the three after the wait are [`crate::term::Term::suspend`]
//! and [`crate::term::Term::resume`], written once in `src/term/` because the
//! statics that make the restore idempotent live there. This module is the
//! middle: build the argv, create the file for `Shift+F4`, spawn it on the
//! **real** stdio, and wait.
//!
//! **Inherited stdio is the load-bearing word.** The console owns
//! a pty of its own, and an editor spawned onto *that* would draw a full screen
//! into a buffer nobody is looking at. `F4` is not the console: the child gets
//! this process's own stdin, stdout and stderr, which are the user's terminal.
//! [`Stdio::inherit`] is already `Command`'s default; it is written out because
//! it is the requirement rather than an accident.
//!
//! # Where each half runs
//!
//! `crate::input::dispatch` may not touch the filesystem, stdout or the
//! terminal, and all three are exactly what this is.
//! So the keystroke only *plans*: [`plan`] turns the entry under the cursor
//! into a [`ExternalCommand`] on [`crate::ops::open::Handoff`], and the event loop
//! calls [`service`] one turn later, the way it services a directory read.
//!
//! # Restoring the terminal on every path
//!
//! [`service`] calls `Term::resume` after the child has been waited for
//! **whatever happened** - a clean exit, a non-zero exit, the editor killed by
//! a signal, a spawn that never got off the ground because the program is not
//! on `PATH`. A failed spawn that left the terminal cooked would draw the
//! error message onto a screen that is no longer the application's.
//!
//! A panic *while the editor runs* is covered by the other half of the same
//! design: `Term::restore` is idempotent and the panic hook calls it, so the
//! terminal is already in the state a panic message wants - cooked, on the main
//! screen. Nothing here re-enters the alternate screen on the unwind path, on
//! purpose: doing so would hide the message the hook just printed.
//!
//! # the design is not this milestone
//!
//! Editing anything that is not on the local filesystem - inside an archive, on
//! SFTP - is the temp-file round trip and needs a non-local `Vfs`, which does
//! not exist until v0.5. [`plan`] refuses it **up front**, through
//! `Capabilities`, naming the milestone. That is the own first rule ("refuse
//! early") applied to the case where the whole feature is missing: discovering
//! it after twenty minutes of editing is the failure mode to design out.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

use crate::app::{App, ExternalCommand};
use crate::config::EditorConfig;
use crate::error::Result;
use crate::panel::Side;
use crate::term::Term;
use crate::vfs::{Capabilities, VfsPath};

/// The editor used when `editor.command`, `$VISUAL` and `$EDITOR` are all
/// empty.
pub const FALLBACK_EDITOR: &str = "nano";

/// The mandatory placeholder in `editor.args`.
pub const FILE_PLACEHOLDER: &str = "{file}";

/// The optional placeholder, substituted only when a line is known.
pub const LINE_PLACEHOLDER: &str = "{line}";

/// Why an empty name is refused, phrased for the `Shift+F4` prompt.
pub const EMPTY_NAME: &str = "a file name cannot be empty";

/// Why `F4` will not start an editor at all (the "refuse early").
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The file is not on the local filesystem: the round trip -
    /// download to a temp file, edit, upload on change, with the mtime check.
    ///
    /// the design asks for it on a remote panel and the design asks for it
    /// on an archive member; it is **one** mechanism and building it for
    /// remotes alone would leave archives second-class and would be written
    /// twice. The milestone it names moves with the milestone that will
    /// bring it, and this is the third time it has: v0.5 brought archives,
    /// v0.65 brings remotes, and neither brought the round trip.
    NotLocal,
    /// The backend cannot be written to at all.
    ReadOnly,
    /// The path is not valid UTF-8, and [`ExternalCommand::args`] is
    /// `Vec<String>`. Refused rather than lossily converted: a lossy path names
    /// a *different* file, and the editor would silently create it.
    NotUtf8(PathBuf),
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // **No version in this message.** It used to name v0.7, which has
            // now shipped without the round trip, and the design
            // has no line after v0.7 to name instead - so it says what is true
            // and names nothing, on the same reasoning
            // the design applies to the SSH console.
            Self::NotLocal => write!(
                f,
                "editing a file that is not on the local filesystem is not built \
; copy it here with F5 first"
            ),
            Self::ReadOnly => write!(f, "this backend is read-only; nothing here can be edited"),
            Self::NotUtf8(path) => write!(
                f,
                "{}: this name is not valid UTF-8 and cannot be handed to an editor",
                path.display()
            ),
        }
    }
}

/// Is this something the `Shift+F4` prompt may accept?
///
/// The dialog already refuses an empty field; this states the same rule where a
/// caller that is not a dialog can reach it, and adds the two a text field
/// cannot see. A trailing `/` is refused because it names a directory, and
/// `F7` is the key that makes one.
pub fn validate_name(name: &str) -> std::result::Result<(), &'static str> {
    let name = name.trim();
    if name.is_empty() {
        return Err(EMPTY_NAME);
    }
    if name.contains('\0') {
        return Err("a file name cannot contain a null byte");
    }
    if name.ends_with('/') {
        return Err("that names a directory; F7 creates one");
    }
    Ok(())
}

/// Split a command string into words, the way a shell would.
///
/// `$EDITOR` is a *command line*, not a program name: `emacs -nw`, `code -w`
/// and `nvim -u NONE` are all ordinary values of it, and the design requires
/// `emacs -nw` to work "with no special-casing". Splitting is that generality -
/// the alternative is a list of editors that get an exception, which is exactly
/// what the spec forbids.
///
/// Quotes and backslashes are honoured so a program under a path with a space
/// survives; nothing else a shell does (expansion, globbing, operators) is
/// interpreted, because this is not a shell and the value is not going through
/// one.
pub fn split_command(raw: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut started = false;
    let mut quote: Option<char> = None;
    let mut escaped = false;

    for ch in raw.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        match (quote, ch) {
            (None, '\\') => {
                escaped = true;
                started = true;
            }
            (None, '\'' | '"') => {
                quote = Some(ch);
                started = true;
            }
            (Some(q), c) if c == q => quote = None,
            // A backslash inside single quotes is literal, as in a shell.
            (Some('\''), c) => current.push(c),
            (Some(_), '\\') => escaped = true,
            (Some(_), c) => current.push(c),
            (None, c) if c.is_whitespace() => {
                if started {
                    words.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            (None, c) => {
                current.push(c);
                started = true;
            }
        }
    }
    if started {
        words.push(current);
    }
    words
}

/// The editor's words: `editor.command`, else `$VISUAL`, else `$EDITOR`, else
/// `nano`.
///
/// The environment is passed in rather than read here so the rule is testable:
/// `std::env::set_var` is `unsafe` in edition 2024 and a test that mutates the
/// process environment is a test that races every other test in the binary.
pub fn program_words(configured: &str, visual: Option<&str>, editor: Option<&str>) -> Vec<String> {
    for candidate in [configured, visual.unwrap_or(""), editor.unwrap_or("")] {
        let words = split_command(candidate);
        if !words.is_empty() {
            return words;
        }
    }
    vec![FALLBACK_EDITOR.to_string()]
}

/// [`program_words`], reading the environment.
pub fn program_words_from_env(configured: &str) -> Vec<String> {
    let visual = std::env::var("VISUAL").ok();
    let editor = std::env::var("EDITOR").ok();
    program_words(configured, visual.as_deref(), editor.as_deref())
}

/// Substitute `{file}` and `{line}` through an argument template.
///
///
/// * **`{line}` is substituted only when a line is known.** An argument that
///   mentions it with no line is dropped *whole*, because the template that
///   makes this feature worth having is `["+{line}", "{file}"]` and `nvim +`
///   with an empty line number is an error rather than a default.
/// * **`{file}` is mandatory**, and mandatory is enforced by *supplying* it:
///   when no emitted argument mentions it, the path is appended. A template
///   that forgot it would otherwise open an empty buffer, and an `F4` that
///   silently edits nothing is worse than one that ignores a misconfiguration.
pub fn expand_args(template: &[String], file: &str, line: Option<u64>) -> Vec<String> {
    let mut out = Vec::with_capacity(template.len() + 1);
    let mut saw_file = false;
    for raw in template {
        let arg = if raw.contains(LINE_PLACEHOLDER) {
            match line {
                Some(line) => raw.replace(LINE_PLACEHOLDER, &line.to_string()),
                // No line: the whole argument goes, `{file}` in it included.
                None => continue,
            }
        } else {
            raw.clone()
        };
        saw_file |= arg.contains(FILE_PLACEHOLDER);
        out.push(arg.replace(FILE_PLACEHOLDER, file));
    }
    if !saw_file {
        out.push(file.to_string());
    }
    out
}

/// Plan the editor for one file.
///
/// Everything a keystroke is allowed to do: no process is started, no file is
/// touched, and the terminal is not disturbed. `caps` comes from the path's own
/// backend, so a panel showing search results over local files is editable
/// while an archive is refused.
///
/// `line` is `None` everywhere in v0.3 - the example of a known one
/// is "when `F4` is pressed on a search result", and search is v0.6. The
/// parameter is here so that arrives as a call-site change rather than as a
/// change to the templating.
pub fn plan(
    cfg: &EditorConfig,
    file: &VfsPath,
    caps: Capabilities,
    line: Option<u64>,
    follow: Side,
) -> std::result::Result<ExternalCommand, Refusal> {
    let Some(local) = file.local_path() else {
        return Err(Refusal::NotLocal);
    };
    if !caps.writable {
        return Err(Refusal::ReadOnly);
    }
    let Some(text) = local.to_str() else {
        return Err(Refusal::NotUtf8(local.to_path_buf()));
    };

    let mut words = program_words_from_env(&cfg.command).into_iter();
    // `program_words` never yields an empty list, and this is the shape that
    // says so without an `expect`.
    let program = words.next().unwrap_or_else(|| FALLBACK_EDITOR.to_string());
    let mut args: Vec<String> = words.collect();
    args.extend(expand_args(&cfg.args, text, line));

    Ok(ExternalCommand {
        program,
        args,
        // The editor runs in the file's own directory, so a `:e ../other`
        // inside it means what the user sees in the panel.
        cwd: local.parent().map(VfsPath::local),
        follow: Some(follow),
    })
}

/// Create the file `Shift+F4` was given a name for.
///
/// `create`, not `create_new`: a name that already exists is opened rather than
/// refused, which is what a user typing a name they half-remember means, and
/// the file is never truncated. This is `touch`, and it runs in the event loop
/// because it is a filesystem call.
pub fn touch(path: &Path) -> Result<()> {
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map(|_| ())
        .map_err(|source| crate::error::Error::io(path, source))
}

/// Spawn the editor on the real terminal and wait for it.
///
/// The caller must already have suspended the terminal: this hands the child
/// this process's own stdin, stdout and stderr, and a child drawing a full
/// screen into a terminal that is still in raw mode on the alternate screen is
/// the bug this whole module is arranged to avoid.
pub fn spawn_and_wait(cmd: &ExternalCommand) -> io::Result<ExitStatus> {
    let mut command = Command::new(&cmd.program);
    command
        .args(&cmd.args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    // Only a directory that is actually there: `current_dir` on a missing one
    // fails the *spawn*, which would be reported as "the editor could not be
    // started" and send the user looking for the wrong problem.
    if let Some(dir) = cmd.cwd.as_ref().and_then(VfsPath::local_path)
        && dir.is_dir()
    {
        command.current_dir(dir);
    }
    command.spawn()?.wait()
}

/// The whole command line that was tried, for an error message.
///
/// the design leaves the wording open; naming the program *and* what it was
/// given is what makes "not on PATH" distinguishable from "the template is
/// wrong" without a debug build.
pub fn command_line(cmd: &ExternalCommand) -> String {
    let mut line = cmd.program.clone();
    for arg in &cmd.args {
        line.push(' ');
        line.push_str(arg);
    }
    line
}

/// Why the editor never started, phrased for the status line.
pub fn spawn_failure(cmd: &ExternalCommand, err: &io::Error) -> String {
    let tried = command_line(cmd);
    match err.kind() {
        io::ErrorKind::NotFound => {
            format!("{tried}: no such program - check `editor.command` in config.toml")
        }
        io::ErrorKind::PermissionDenied => format!("{tried}: not executable"),
        _ => format!("{tried}: {err}"),
    }
}

/// How the editor ended, when it did not end cleanly.
fn describe(status: ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("exited with status {code}"),
        None => format!("ended abnormally ({status})"),
    }
}

/// Create the file if asked, run the editor, and say what went wrong.
///
/// `None` is "nothing to report". Split out from [`service`] so the part that
/// needs no terminal can be exercised on its own.
fn create_and_run(create: Option<&Path>, cmd: &ExternalCommand) -> Option<String> {
    if let Some(path) = create
        && let Err(err) = touch(path)
    {
        // The editor is not started: a `Shift+F4` whose file could not be
        // created would otherwise open a buffer that cannot be saved either.
        return Some(err.to_string());
    }
    match spawn_and_wait(cmd) {
        Ok(status) if status.success() => None,
        Ok(status) => Some(format!("{}: {}", cmd.program, describe(status))),
        Err(err) => Some(spawn_failure(cmd, &err)),
    }
}

/// Run the queued external command, if there is one.
///
/// **This is the event loop's single call.** It performs the whole hand-over:
///
/// 1. `Term::suspend` - leave the alternate screen, cooked mode, pop the flags.
/// 2. Create the file, when `Shift+F4` named one.
/// 3. Spawn with inherited stdio and wait.
/// 4. `Term::resume` - alternate screen, raw mode, flags, and the full redraw.
///    **Unconditionally**, before any message is composed.
/// 5. Re-read the panel `ExternalCommand::follow` names, which is what puts the
///    edited file back on screen with its new size; the cursor stays on it
///    through `Tab::pending_select`, set when the keystroke planned this.
///
/// The file to create is read from [`crate::app::jobs::draft::JobDraft::sources`], which is where a
/// keystroke leaves the operands of the operation it is queueing. `F4` clears
/// it, so a stale value from an abandoned dialog can never be created by
/// accident. See this milestone's "needs" note: a `create` field on
/// [`ExternalCommand`] would say it more plainly, and is a two-line change here.
pub fn service(app: &mut App, term: &mut Term) -> Result<()> {
    let Some(cmd) = app.handoff.external.take() else {
        return Ok(());
    };
    let create = app
        .draft
        .sources
        .first()
        .and_then(|path| path.local_path().map(Path::to_path_buf));
    app.draft.sources.clear();

    // the design steps 1-3.
    term.suspend()?;
    let failure = create_and_run(create.as_deref(), &cmd);
    // Steps 6-8, on every path - including the one where the spawn failed, so
    // the message below is drawn on the application's own screen.
    term.resume()?;

    if let Some(message) = failure {
        app.message = Some(message);
    }
    if let Some(side) = cmd.follow {
        app.reread(side);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(command: &str, args: &[&str]) -> EditorConfig {
        EditorConfig {
            warn_above: crate::config::ByteSize::mib(10),
            command: command.to_string(),
            args: args.iter().map(|a| (*a).to_string()).collect(),
        }
    }

    fn local(path: &str) -> VfsPath {
        VfsPath::local(path)
    }

    #[test]
    fn the_default_config_hands_the_file_to_whatever_the_environment_names() {
        // `EditorConfig::default().command` is **empty**, which is the "`$VISUAL` then
        // `$EDITOR` are consulted when the config value is empty" - so which program this
        // resolves to depends on the machine the test is running on, and asserting `nano`
        // here would only be asserting that the developer has no `$EDITOR` set. The chain
        // itself is pinned by
        // `the_environment_is_consulted_only_when_the_config_value_is_empty`, which takes
        // the environment as an argument.
        //
        // What is the same on every machine is everything else the plan says.
        let plan = plan(
            &EditorConfig::default(),
            &local("/home/t/notes.txt"),
            Capabilities::LOCAL,
            None,
            Side::Left,
        )
        .expect("a local file is editable");
        assert!(!plan.program.is_empty(), "some editor is always named");
        // `$EDITOR` holds a *command line*, so it may contribute arguments of
        // its own ahead of the template's - `code -w`, `emacs -nw`. What is
        // invariant is that the file arrives, last, unquoted and whole.
        assert_eq!(
            plan.args.last().map(String::as_str),
            Some("/home/t/notes.txt")
        );
        assert_eq!(plan.cwd, Some(local("/home/t")));
        assert_eq!(plan.follow, Some(Side::Left));
    }

    #[test]
    fn nothing_configured_and_nothing_in_the_environment_is_nano() {
        // The end of the chain, and the one link that cannot be
        // observed through `plan` without owning the environment.
        assert_eq!(
            program_words(&EditorConfig::default().command, None, None),
            [FALLBACK_EDITOR]
        );
    }

    #[test]
    fn every_editor_spec_names_works_with_no_special_casing() {
        // "Any of `nvim`, `vim`, `vi`, `helix`, `micro`,
        // `emacs -nw` must work with no special-casing." The only one that is
        // not a bare word is `emacs -nw`, and it is a *command line* - which is
        // what `$EDITOR` holds on any number of machines.
        for command in ["nvim", "vim", "vi", "helix", "micro"] {
            let plan = plan(
                &cfg(command, &["{file}"]),
                &local("/tmp/a.rs"),
                Capabilities::LOCAL,
                None,
                Side::Right,
            )
            .expect("local");
            assert_eq!(plan.program, command);
            assert_eq!(plan.args, vec!["/tmp/a.rs".to_string()]);
        }

        let plan = plan(
            &cfg("emacs -nw", &["{file}"]),
            &local("/tmp/a.rs"),
            Capabilities::LOCAL,
            None,
            Side::Right,
        )
        .expect("local");
        assert_eq!(plan.program, "emacs");
        assert_eq!(plan.args, vec!["-nw".to_string(), "/tmp/a.rs".to_string()]);
    }

    #[test]
    fn the_line_placeholder_is_dropped_when_no_line_is_known() {
        // "`{line}` substituted only when a line is known".
        let template = cfg("nvim", &["+{line}", "{file}"]);
        let without = plan(
            &template,
            &local("/tmp/a.rs"),
            Capabilities::LOCAL,
            None,
            Side::Left,
        )
        .expect("local");
        assert_eq!(without.args, vec!["/tmp/a.rs".to_string()]);

        let with = plan(
            &template,
            &local("/tmp/a.rs"),
            Capabilities::LOCAL,
            Some(42),
            Side::Left,
        )
        .expect("local");
        assert_eq!(
            with.args,
            vec!["+42".to_string(), "/tmp/a.rs".to_string()],
            "the line argument keeps its place in the template"
        );
    }

    #[test]
    fn an_argument_holding_both_placeholders_goes_whole_and_the_file_still_arrives() {
        // `code -g {file}:{line}` is the shape. With no line the argument is
        // meaningless, so it is dropped - and `{file}` being mandatory means
        // the path is appended rather than lost.
        let args = expand_args(&["-g".into(), "{file}:{line}".into()], "/tmp/a.rs", None);
        assert_eq!(args, vec!["-g".to_string(), "/tmp/a.rs".to_string()]);

        let args = expand_args(&["-g".into(), "{file}:{line}".into()], "/tmp/a.rs", Some(7));
        assert_eq!(args, vec!["-g".to_string(), "/tmp/a.rs:7".to_string()]);
    }

    #[test]
    fn a_template_that_forgot_the_file_still_gets_it() {
        assert_eq!(
            expand_args(&["-R".into()], "/tmp/a.rs", None),
            vec!["-R".to_string(), "/tmp/a.rs".to_string()]
        );
        assert_eq!(
            expand_args(&[], "/tmp/a.rs", None),
            vec!["/tmp/a.rs".to_string()]
        );
    }

    #[test]
    fn the_environment_is_consulted_only_when_the_config_value_is_empty() {
        // "`$VISUAL` then `$EDITOR` are consulted when the
        // config value is empty."
        assert_eq!(program_words("nano", Some("vim"), Some("emacs")), ["nano"]);
        assert_eq!(program_words("", Some("vim"), Some("emacs")), ["vim"]);
        assert_eq!(program_words("", None, Some("emacs -nw")), ["emacs", "-nw"]);
        assert_eq!(program_words("   ", None, None), [FALLBACK_EDITOR]);
        assert_eq!(program_words("", Some(""), Some("")), [FALLBACK_EDITOR]);
    }

    #[test]
    fn a_program_under_a_path_with_a_space_survives_splitting() {
        assert_eq!(
            split_command("\"/opt/My Editor/bin/ed\" -x"),
            ["/opt/My Editor/bin/ed", "-x"]
        );
        assert_eq!(split_command("/opt/My\\ Editor/ed"), ["/opt/My Editor/ed"]);
        assert_eq!(split_command("  vim   -u  NONE "), ["vim", "-u", "NONE"]);
        assert_eq!(split_command(""), Vec::<String>::new());
        assert_eq!(split_command("''"), [""], "an empty quoted word is a word");
    }

    #[test]
    fn a_file_that_is_not_local_is_refused_up_front_naming_the_milestone() {
        // the design is the temp-file round trip and needs a non-local
        // `Vfs`; refusing early is the own first rule.
        let inside = VfsPath::new(crate::vfs::BackendKind::List, "/results/a.rs");
        let refusal = plan(
            &EditorConfig::default(),
            &inside,
            crate::vfs::BackendKind::List.capabilities(),
            None,
            Side::Left,
        )
        .expect_err("not a local path");
        assert_eq!(refusal, Refusal::NotLocal);
        assert!(
            !refusal.to_string().contains("v0."),
            "the refusal names no release: v0.7 has shipped without the design \
             11.2's round trip and the design has nothing after it: {refusal}"
        );
        assert!(
            refusal.to_string().contains("F5"),
            "and it says what to do instead: {refusal}"
        );
    }

    #[test]
    fn a_read_only_backend_is_refused_before_anything_is_started() {
        let caps = Capabilities {
            writable: false,
            ..Capabilities::LOCAL
        };
        let refusal = plan(
            &EditorConfig::default(),
            &local("/tmp/a.rs"),
            caps,
            None,
            Side::Left,
        )
        .expect_err("read-only");
        assert_eq!(refusal, Refusal::ReadOnly);
        assert!(refusal.to_string().contains("read-only"));
    }

    #[test]
    fn names_the_prompt_must_refuse() {
        assert_eq!(validate_name(""), Err(EMPTY_NAME));
        assert_eq!(validate_name("   "), Err(EMPTY_NAME));
        assert!(validate_name("a\0b").is_err());
        assert!(validate_name("photos/").is_err());
        assert!(validate_name("notes.txt").is_ok());
        assert!(validate_name("sub/notes.txt").is_ok());
    }

    #[test]
    fn a_missing_program_says_what_was_tried() {
        let cmd = ExternalCommand {
            program: "hcmd-no-such-editor".to_string(),
            args: vec!["/tmp/a.rs".to_string()],
            cwd: None,
            follow: Some(Side::Left),
        };
        let err = spawn_and_wait(&cmd).expect_err("no such program");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        let message = spawn_failure(&cmd, &err);
        assert!(
            message.contains("hcmd-no-such-editor /tmp/a.rs"),
            "{message}"
        );
        assert!(message.contains("editor.command"), "{message}");
    }

    #[test]
    fn a_real_program_is_spawned_and_waited_for() {
        // `/bin/true` and `/bin/false` are the smallest editors there are.
        let mut cmd = ExternalCommand {
            program: "true".to_string(),
            args: Vec::new(),
            cwd: Some(local("/")),
            follow: None,
        };
        let status = spawn_and_wait(&cmd).expect("true is on PATH");
        assert!(status.success());
        assert_eq!(create_and_run(None, &cmd), None);

        cmd.program = "false".to_string();
        let message = create_and_run(None, &cmd).expect("a non-zero exit is reported");
        assert!(message.contains("exited with status 1"), "{message}");
    }

    #[test]
    fn shift_f4_creates_the_file_and_leaves_an_existing_one_alone() {
        let dir = std::env::temp_dir().join(format!("hcmd-editor-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("new file.txt");
        let _ = std::fs::remove_file(&path);

        let cmd = ExternalCommand {
            program: "true".to_string(),
            args: vec![path.to_string_lossy().into_owned()],
            cwd: Some(VfsPath::local(&dir)),
            follow: Some(Side::Left),
        };
        assert_eq!(create_and_run(Some(&path), &cmd), None);
        assert!(path.is_file(), "Shift+F4 creates the file");

        std::fs::write(&path, b"already here").expect("write");
        assert_eq!(create_and_run(Some(&path), &cmd), None);
        assert_eq!(
            std::fs::read(&path).expect("read"),
            b"already here",
            "a name that already exists is opened, never truncated"
        );

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn a_file_that_cannot_be_created_stops_before_the_editor_runs() {
        let path = Path::new("/hcmd-no-such-directory/notes.txt");
        let cmd = ExternalCommand {
            program: "hcmd-no-such-editor".to_string(),
            args: vec![path.to_string_lossy().into_owned()],
            cwd: None,
            follow: None,
        };
        // The message is the *creation* failure, which means the spawn was
        // never attempted - the editor's own "no such program" would have won.
        let message = create_and_run(Some(path), &cmd).expect("refused");
        assert!(message.contains("notes.txt"), "{message}");
        assert!(!message.contains("hcmd-no-such-editor"), "{message}");
    }
}
