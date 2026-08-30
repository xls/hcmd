//! Opening and executing.
//!
//! `Enter` on a file that is not a directory and not an archive ends here, and
//! so does `Shift+Enter`, which "**always** opens with the associated
//! application, never executes".
//!
//! # Everything here is in process
//!
//! the design settles that "archives, search, device enumeration and file
//! associations are all in-process crates". So the MIME type is
//! [`mime_guess`] on the name corrected by [`infer`] on the bytes, the
//! "Open with..." list is `freedesktop-desktop-entry` reading the desktop entry
//! directories, and nothing asks `xdg-mime` or parses the output of a
//! subprocess. `xdg-mime` was considered and rejected by name in.
//!
//! # The one thing that *is* a subprocess, and why it is not an exception
//!
//! Launching the program the user asked to launch **is the feature**, not an
//! implementation of it - `F4` has spawned an external editor since v0.3.
//! What is forbidden is the *shell*: `$VAR` and `${VAR}` are
//! expanded by [`expand_command`] from the process environment, the words are
//! already split by TOML, and no `sh -c` is ever constructed.
//!
//!
//! The single `$SHELL` invocation in the tree is the own
//! instruction - "a script with a shebang is exec'd normally; one **without** a
//! shebang is run through `$SHELL`" - and [`execute_argv`] passes the file as a
//! single argument, never as a command string.
//!
//! # Which half runs where
//!
//! [`kind_of`], [`mime_of`], [`resolve`], [`expand_command`], [`is_executable`]
//! and [`execute_argv`] are **pure**: they are handed the window of bytes the
//! caller already read and they touch nothing. [`applications_for`],
//! [`desktop_open`] and [`spawn_detached`] read directories or start processes
//! and are therefore the event loop's, never `crate::input::dispatch`'s.
//!

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use crate::app::ExternalCommand;
use crate::config::{Matcher, ModeMatch, OpenConfig};
use crate::vfs::VfsPath;

/// How many bytes of a file [`kind_of`] and [`mime_of`] want to see.
///
/// Not a limit, a **floor**: `infer`'s ELF matcher refuses to answer on fewer
/// than 53 bytes, and a caller that reads a smaller window would be told a
/// binary is `data`. The event loop reads one window through the `Vfs` and
/// passes it to both functions and to the shebang test, so there is one read
/// per `Enter` (the design step 2).
pub const HEAD_WINDOW: usize = 512;

/// The MIME type used when neither the name nor the bytes say anything.
///
/// The Desktop Entry Specification's own "unknown" type, so a handler written
/// as `mime = "application/octet-stream"` means what a reader expects.
pub const UNKNOWN_MIME: &str = "application/octet-stream";

/// the refusal for a file the kernel cannot exec, naming the key
/// that fixes it.
///
/// > A file inside an archive or on an SFTP/FTP/S3 panel has no path the kernel
/// > can exec, and silently copying it to a temp directory to run it is a
/// > security decision the user did not make. The message says to copy it out
/// > with `F5` first.
pub const NOT_LOCAL: &str =
    "this file is not on the local filesystem and cannot be run; copy it out with F5 first";

/// The shell used when `$SHELL` is unset, for the design's
/// "one **without** a shebang is run through `$SHELL`".
pub const FALLBACK_SHELL: &str = "/bin/sh";

/// The placeholder a `[[open.handlers]]` command substitutes the file into.
///
pub const FILE_PLACEHOLDER: &str = "{file}";

/// The mode bits' file-type field, `S_IFMT`.
///
/// Spelled out here rather than taken from `libc`: `#![forbid(unsafe_code)]`
/// rules out `libc`'s API surface generally, and these three numbers have been
/// fixed since the first Unix.
const S_IFMT: u32 = 0o170_000;
/// `S_IFDIR`.
const S_IFDIR: u32 = 0o040_000;
/// `S_IFLNK`.
const S_IFLNK: u32 = 0o120_000;

/// What the resolution order picked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Association {
    /// 1. A user handler from `config.toml`, already expanded.
    Handler(ExternalCommand),
    /// 2. The desktop's default, launched through the `open` crate.
    Desktop(PathBuf),
    /// 3. The internal viewer, "so `Enter` on an unknown type shows the file
    ///    rather than doing nothing".
    Viewer(VfsPath),
}

/// One application the desktop advertises for a type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopApp {
    /// The `Name=` field, which is what the chooser lists.
    pub name: String,
    /// The desktop entry's id, so two applications with one name are still
    /// two rows.
    pub id: String,
    /// `Exec=`, split and with its field codes removed.
    pub exec: Vec<String>,
}

/// What the file actually is, from its content (the prompt names
/// it): `ELF binary`, `shell script`, `Python script`, `data`.
///
/// [`infer`] first, then the shebang line, then the extension. **Never the
/// extension alone** for anything that decides behaviour: the design is
/// explicit that executability and type are content questions, and the
/// extension is consulted here only to put a better word than `data` in front
/// of a user who is being asked whether to run something.
pub fn kind_of(head: &[u8], name: &str) -> String {
    // A shebang before `infer`, even though the contract writes them the other
    // way round, for one reason and only one: `infer`'s shell-script matcher
    // *is* "starts with `#!`", so it answers `shell script` for a Python
    // script too. The shebang line names the interpreter, which is strictly
    // more information from the same two bytes, and the design asks the
    // prompt to distinguish exactly those two cases ("ELF binary, shell
    // script, Python script"). Every other content type still goes to `infer`
    // below, so the ordering costs nothing.
    if let Some(kind) = shebang_kind(head) {
        return kind;
    }
    if let Some(found) = infer::get(head) {
        return infer_kind(found);
    }
    if let Some(kind) = extension_kind(name) {
        return kind;
    }
    if looks_like_text(head) {
        return "text".to_string();
    }
    "data".to_string()
}

/// The MIME type: [`mime_guess`] on the extension, **corrected by [`infer`] on
/// the content when the two disagree**.
///
/// > a `.txt` that is actually a JPEG opens as an image
///
/// Content wins wherever `infer` recognised the bytes at all, which is
/// invariant I13. The one visible consequence of taking that rule literally is
/// that `infer`'s shell-script matcher is "starts with `#!`", so a Python
/// script with a shebang reports `text/x-shellscript` rather than
/// `text/x-python`. Both are `text/*`, which is what a `[[open.handlers]]`
/// rule written the way the own example writes one matches on,
/// and [`kind_of`] - the function whose answer a *user* reads - reads the
/// interpreter off the shebang line instead.
pub fn mime_of(name: &str, head: &[u8]) -> String {
    let from_content = infer::get(head).map(|found| found.mime_type().to_string());
    if let Some(mime) = from_content {
        return mime;
    }
    mime_guess::from_path(name)
        .first_raw()
        .map_or_else(|| UNKNOWN_MIME.to_string(), str::to_string)
}

/// executability is the mode bits and nothing else.
///
/// Invariant I16. There is no extension anywhere in this function on purpose:
/// "A `.sh` without `+x` is data; a file with no extension and `+x` is a
/// program."
pub const fn is_executable(mode: u32) -> bool {
    mode & 0o111 != 0
}

/// the resolution order, first match wins.
///
/// 1. **User handlers** from `config.toml`, in file order - "the TC model,
///    where your own rules beat the desktop's".
/// 2. **The desktop's default**, through the `open` crate.
/// 3. **The internal viewer**, "so `Enter` on an unknown type shows the file
///    rather than doing nothing".
///
/// Pure: `head` is the first window the caller already read, and nothing in
/// here opens a file. `mode` is the entry's mode bits, for a handler whose
/// `match` names `mode`.
///
/// A path that is **not** local skips step 2 and lands on the viewer, because
/// the desktop cannot be handed a file inside an archive or on a remote host;
/// the temp-file round trip for those is the, which is not built. That is the
/// same refusal [`crate::ops::editor::plan`] already makes and it is why step
/// 3 exists.
pub fn resolve(cfg: &OpenConfig, path: &VfsPath, head: &[u8], mode: u32) -> Association {
    let name = path.file_name().unwrap_or_default();
    let mime = mime_of(&name, head);
    let local = path.local_path();

    for handler in &cfg.handlers {
        if !matches(&handler.matcher, &name, &mime, mode) {
            continue;
        }
        // A handler with an empty `command` is **inert**, and the check is
        // here rather than left to `expand_command`: that function appends the
        // path when no word mentioned `{file}`, so an empty template would
        // come back as the file itself and the file would become argv[0] -
        // which is executing something the rule never named. the design makes
        // a bad value in a config file never fatal, so the rule is skipped.
        if handler.command.is_empty() {
            continue;
        }
        // A handler needs a path the kernel can be handed. Inside an archive
        // or on a remote host there is none, and the answer for those - the
        // temp-file round trip - is not built, so the resolution falls
        // through to the internal viewer below rather than launching
        // something on a path that does not exist.
        let Some(argv_source) = local else {
            continue;
        };
        let argv = expand_command(&handler.command, argv_source);
        let mut words = argv.into_iter();
        let Some(program) = words.next() else {
            continue;
        };
        return Association::Handler(ExternalCommand {
            program,
            args: words.collect(),
            // Filled in by the event loop, which is the half that knows which
            // panel the key came from; `resolve` is pure and has no side.
            cwd: None,
            follow: None,
        });
    }

    match local {
        Some(local) => Association::Desktop(local.to_path_buf()),
        None => Association::Viewer(path.clone()),
    }
}

/// Expand `{file}` and `$VAR` / `${VAR}` in a handler's argv.
///
/// **No shell is constructed.** The words are already split by TOML; this
/// substitutes into them.
///
/// One left-to-right pass per word, so a variable whose *value* contains
/// `{file}` or another `$` is not expanded again - which is the whole
/// difference between substitution and a shell.
///
/// An unset variable expands to nothing, as a shell would. A template that
/// never mentions `{file}` gets the path appended, for the reason
/// [`crate::ops::editor::expand_args`] does the same: a handler that opened an
/// empty window would be a misconfiguration nobody could see.
pub fn expand_command(template: &[String], file: &Path) -> Vec<String> {
    let path = file.to_string_lossy();
    let mut out = Vec::with_capacity(template.len().saturating_add(1));
    let mut saw_file = false;
    for word in template {
        let (expanded, had_file) = expand_word(word, &path);
        saw_file |= had_file;
        out.push(expanded);
    }
    if !saw_file {
        out.push(path.into_owned());
    }
    out
}

/// The argv for running an executable by **absolute path**.
///
/// > A script with a shebang is exec'd normally; one **without** a shebang is
/// > run through `$SHELL`.
///
/// Never relies on `PATH` and never adds `.` to it (invariant I16, I17).
/// Returns the refusal message instead when the path is not local, naming
/// `F5`.
///
/// **A file with no shebang is only handed to `$SHELL` when it looks like
/// text.** the sentence is about *scripts*; an ELF binary has no
/// shebang either, and running one through a shell would either fail or, worse,
/// have the shell try to interpret it. So the shell is used for a file with no
/// NUL byte in its first window and nothing else, which is the same "is this
/// text" question the viewer asks.
pub fn execute_argv(
    path: &VfsPath,
    head: &[u8],
    shell: Option<&OsStr>,
) -> std::result::Result<Vec<String>, String> {
    let Some(local) = path.local_path() else {
        return Err(NOT_LOCAL.to_string());
    };
    let Some(text) = local.to_str() else {
        return Err(format!(
            "{}: this name is not valid UTF-8 and cannot be handed to the kernel",
            local.display()
        ));
    };
    if head.starts_with(b"#!") || !looks_like_text(head) {
        return Ok(vec![text.to_string()]);
    }
    let shell = shell
        .and_then(OsStr::to_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(FALLBACK_SHELL);
    // The file as a **single argument**, never as a command string: a
    // `sh -c "<path>"` would re-parse a filename containing a space or a
    // semicolon as shell syntax.
    Ok(vec![shell.to_string(), text.to_string()])
}

/// The applications the desktop advertises for `mime`, read with
/// `freedesktop-desktop-entry` (the "Open with...").
///
/// Reads the desktop entry directories, so it is the event loop's, never
/// `dispatch`'s. An empty list is a normal answer on a machine with no desktop
/// installed, and the chooser says so rather than showing nothing
/// (`crate::ui::dialog::OpenWithDialog`).
///
/// Entries marked `NoDisplay=true` or `Hidden=true` are left out: the first
/// means "not for a menu" and this is a menu, the second means the entry has
/// been deleted by a later directory in the search path.
pub fn applications_for(mime: &str) -> Vec<DesktopApp> {
    let locales = freedesktop_desktop_entry::get_languages_from_env();
    let mut out: Vec<DesktopApp> = Vec::new();
    let iter = freedesktop_desktop_entry::Iter::new(desktop_entry_dirs().into_iter());
    for entry in iter.entries(Some(&locales)) {
        if entry.no_display() || entry.hidden() {
            continue;
        }
        if !entry
            .mime_type()
            .is_some_and(|types| types.iter().any(|pattern| mime_matches(pattern, mime)))
        {
            continue;
        }
        let Ok(exec) = entry.parse_exec() else {
            continue;
        };
        if exec.is_empty() {
            continue;
        }
        let id = entry.appid.to_string();
        // The same application id can appear in more than one directory -
        // `~/.local/share` shadowing `/usr/share` is the documented way to
        // override one - and the first directory searched is the one that
        // wins, exactly as the Desktop Entry Specification says.
        if out.iter().any(|app| app.id == id) {
            continue;
        }
        let name = entry
            .name(&locales)
            .map_or_else(|| id.clone(), |n| n.into_owned());
        out.push(DesktopApp { name, id, exec });
    }
    // Alphabetical, because the search order above is a *precedence* order and
    // is not something a reader of the list can see. Case-insensitive, so
    // `Firefox` and `evince` do not sort into two blocks.
    out.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.id.cmp(&b.id))
    });
    out
}

/// The argv that launches `app` on `file` (the "Open with...").
///
/// [`DesktopApp::exec`] already has its field codes removed by
/// `freedesktop-desktop-entry`, so the path is appended rather than
/// substituted: `%f`, `%F`, `%u` and `%U` all mean "the file(s)", and every
/// other `%` code is one the Desktop Entry Specification says an
/// implementation that does not support it must drop.
///
/// Not in the list. It is here because the design describes this step in
/// prose and the event loop has to spell it somewhere; putting it beside
/// [`applications_for`] keeps the two halves of one rule in one file.
pub fn open_with_argv(app: &DesktopApp, file: &Path) -> Vec<String> {
    let mut argv = app.exec.clone();
    argv.push(file.to_string_lossy().into_owned());
    argv
}

/// Hand a path to the desktop's default application.
///
/// `open::that_detached`, so hcmd is not left waiting on a GUI program and the
/// terminal is not handed over. A handler or an execute with
/// `execute_in = "console"` is a different path and does not come through here.
pub fn desktop_open(path: &Path) -> std::io::Result<()> {
    open::that_detached(path)
}

/// Spawn a program with no terminal and no wait (the design's
/// `execute_in = "detached"`).
///
/// Null stdio and no `wait`. This is not a `setsid` daemonisation: that needs
/// `libc` and `unsafe`, and `#![forbid(unsafe_code)]` rules both out. The child
/// is reparented when hcmd exits, which is what the `open` crate's own detached
/// spawn does and is what "suits GUI programs but discards output" means in
/// practice.
pub fn spawn_detached(argv: &[String], cwd: Option<&Path>) -> std::io::Result<()> {
    let Some((program, args)) = argv.split_first() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "nothing to run",
        ));
    };
    let mut command = std::process::Command::new(program);
    command
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }
    command.spawn().map(|_child| ())
}

// ----------------------------------------------------------- internals ------

/// Does one `[[open.handlers]]` rule match?
///
/// A rule may constrain the extension, the MIME type, the mode bits, or any
/// combination, and a combination means **all** of it must hold.
/// A rule that constrains nothing matches
/// nothing, so an empty `match = {}` is inert rather than claiming every file -
/// the same rule `crate::ui::filetype` applies to `[[filetypes]]`, and for the
/// same reason.
fn matches(matcher: &Matcher, name: &str, mime: &str, mode: u32) -> bool {
    let mut said_something = false;

    if let Some(want) = matcher.mode {
        said_something = true;
        let ok = match want {
            ModeMatch::Exec => is_executable(mode),
            ModeMatch::Dir => mode & S_IFMT == S_IFDIR,
            ModeMatch::Symlink => mode & S_IFMT == S_IFLNK,
        };
        if !ok {
            return false;
        }
    }

    if !matcher.ext.is_empty() {
        said_something = true;
        let ext = extension_of(name);
        if ext.is_empty()
            || !matcher
                .ext
                .iter()
                .any(|candidate| candidate.trim_start_matches('.').eq_ignore_ascii_case(ext))
        {
            return false;
        }
    }

    if let Some(pattern) = &matcher.mime {
        said_something = true;
        if !mime_matches(pattern, mime) {
            return false;
        }
    }

    said_something
}

/// A MIME pattern with a trailing `*` allowed: `text/*` matches `text/plain`.
///
/// The same syntax the own example uses, and the same syntax a
/// desktop entry's `MimeType=` may carry, so one function answers both.
/// Case-insensitive, because MIME types are.
fn mime_matches(pattern: &str, mime: &str) -> bool {
    let pattern = pattern.trim();
    if pattern.is_empty() {
        return false;
    }
    match pattern.strip_suffix('*') {
        Some(prefix) => {
            mime.len() >= prefix.len() && mime[..prefix.len()].eq_ignore_ascii_case(prefix)
        }
        None => pattern.eq_ignore_ascii_case(mime),
    }
}

/// The extension of a file name, without the dot, or `""`.
///
/// A leading dot is a hidden file and not an extension, which is why
/// `.bashrc` has none - the same rule `crate::vfs::Entry::split_name` keeps for
/// the `ext` column.
fn extension_of(name: &str) -> &str {
    let trimmed = name.trim_start_matches('.');
    match trimmed.rsplit_once('.') {
        Some((_, ext)) if !ext.is_empty() => ext,
        _ => "",
    }
}

/// One word of a handler's `command`, expanded.
///
/// Returns the word and whether it mentioned [`FILE_PLACEHOLDER`], so
/// [`expand_command`] can tell a template that forgot the file from one that
/// did not.
fn expand_word(word: &str, file: &str) -> (String, bool) {
    let mut out = String::with_capacity(word.len());
    let mut saw_file = false;
    let mut rest = word;
    while !rest.is_empty() {
        if let Some(tail) = rest.strip_prefix(FILE_PLACEHOLDER) {
            out.push_str(file);
            saw_file = true;
            rest = tail;
            continue;
        }
        if let Some(tail) = rest.strip_prefix('$') {
            let (name, tail) = variable_name(tail);
            if name.is_empty() {
                // A lone `$`, or `${` with no closing brace: literal, because
                // there is no variable being named and a shell is not being
                // invented here.
                out.push('$');
                rest = tail;
                continue;
            }
            if let Some(value) = std::env::var_os(name) {
                out.push_str(&value.to_string_lossy());
            }
            rest = tail;
            continue;
        }
        let mut chars = rest.chars();
        if let Some(ch) = chars.next() {
            out.push(ch);
        }
        rest = chars.as_str();
    }
    (out, saw_file)
}

/// Split `$VAR` or `${VAR}` off the front of `rest`, after the `$`.
///
/// Returns `("", rest)` when what follows is not a variable name, which is the
/// signal to emit a literal `$`.
fn variable_name(rest: &str) -> (&str, &str) {
    if let Some(braced) = rest.strip_prefix('{') {
        return match braced.split_once('}') {
            Some((name, tail)) if !name.is_empty() => (name, tail),
            _ => ("", rest),
        };
    }
    let end = rest
        .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .unwrap_or(rest.len());
    // A name may not start with a digit, for the same reason a shell's may
    // not: `$1` is a positional parameter and there are none here.
    if end == 0 || rest.starts_with(|c: char| c.is_ascii_digit()) {
        return ("", rest);
    }
    rest.split_at(end)
}

/// The interpreter named on a `#!` line, turned into the words.
fn shebang_kind(head: &[u8]) -> Option<String> {
    let rest = head.strip_prefix(b"#!")?;
    let line = rest.split(|b| *b == b'\n').next().unwrap_or(rest);
    let line = String::from_utf8_lossy(line);
    // `#!/usr/bin/env python3` names the interpreter in the *second* word, so
    // `env` is stepped over rather than reported as the language.
    let mut words = line
        .split_whitespace()
        .filter(|w| !w.starts_with('-'))
        .map(|w| w.rsplit('/').next().unwrap_or(w));
    let mut interpreter = words.next()?;
    if interpreter == "env" {
        interpreter = words.next()?;
    }
    let lower = interpreter.to_ascii_lowercase();
    let base = lower.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.');
    Some(match base {
        "sh" | "bash" | "zsh" | "dash" | "ksh" | "ash" | "fish" => "shell script".to_string(),
        "python" => "Python script".to_string(),
        "" => "script".to_string(),
        other => format!("{} script", capitalise(other)),
    })
}

/// What [`infer`] recognised, in the shape the prompt wants:
/// a short type name and a category word.
fn infer_kind(found: infer::Type) -> String {
    let category = match found.matcher_type() {
        infer::MatcherType::App => "binary",
        infer::MatcherType::Archive => "archive",
        infer::MatcherType::Audio => "audio",
        infer::MatcherType::Book => "book",
        infer::MatcherType::Doc => "document",
        infer::MatcherType::Font => "font",
        infer::MatcherType::Image => "image",
        infer::MatcherType::Video => "video",
        infer::MatcherType::Text => "text",
        // `infer::MatcherType` is another crate's enum and is not covered by
        // this repository's no-wildcard rule; a variant added by a later
        // release still has to render.
        _ => "file",
    };
    format!("{} {category}", found.extension().to_uppercase())
}

/// The last resort before `data`: what the name suggests.
///
/// Only ever a *label*. Nothing in this module decides whether to run
/// something from an extension (invariant I16).
fn extension_kind(name: &str) -> Option<String> {
    let ext = extension_of(name);
    if ext.is_empty() {
        return None;
    }
    Some(match ext.to_ascii_lowercase().as_str() {
        "sh" | "bash" | "zsh" | "ksh" | "fish" => "shell script".to_string(),
        "py" => "Python script".to_string(),
        "pl" => "Perl script".to_string(),
        "rb" => "Ruby script".to_string(),
        other => format!("{} file", other.to_uppercase()),
    })
}

/// Is this window of bytes text?
///
/// A NUL byte is the test, which is the same one every `file`-like tool and
/// the viewer's own mode choice use. An empty window counts as
/// text: an empty file is a zero-byte script, and handing one to `$SHELL` does
/// nothing, which is the harmless answer.
fn looks_like_text(head: &[u8]) -> bool {
    !head.contains(&0)
}

/// First letter upper-cased, for an interpreter name in a prompt.
fn capitalise(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Where `.desktop` files live, in search order (the XDG base directory
/// specification).
///
/// `freedesktop-desktop-entry::default_paths` does the same thing and is not
/// used: it `unwrap`s the data home, so a process started with no `HOME`
/// panics. rule 5's twenty lines are cheaper than a panic path,
/// and `crate::config::paths` already vendors the same rule for the config
/// directory.
fn desktop_entry_dirs() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    match std::env::var_os("XDG_DATA_HOME") {
        Some(v) if !v.is_empty() && Path::new(&v).is_absolute() => roots.push(PathBuf::from(v)),
        _ => {
            if let Ok(home) = crate::config::paths::home_dir() {
                roots.push(home.join(".local").join("share"));
            }
        }
    }
    let dirs = std::env::var_os("XDG_DATA_DIRS").filter(|v| !v.is_empty());
    match dirs {
        Some(v) => roots.extend(
            std::env::split_paths(&v)
                .filter(|p| p.is_absolute())
                .collect::<Vec<_>>(),
        ),
        None => {
            roots.push(PathBuf::from("/usr/local/share"));
            roots.push(PathBuf::from("/usr/share"));
        }
    }
    roots
        .into_iter()
        .map(|root| root.join("applications"))
        .collect()
}

/// The file this session is about to hand to something outside itself, and
/// nothing else.
///
/// At most one of the three is live, and the alternate screen is left for
/// exactly the duration of whatever gets the file. Nothing here is filled in
/// by [`crate::input::dispatch`]: leaving the alternate screen, restoring
/// cooked mode, popping the keyboard flags, waiting and putting all three back
/// is a terminal operation, so the keystroke queues and the event loop
/// performs, the way it services a read.
///
/// The "at most one" is documented rather than made unrepresentable. An enum
/// would be the better type and is a behaviour change, since two of these
/// could in principle be set today, so it belongs in its own commit rather
/// than inside a decomposition.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Handoff {
    /// An `Enter` on a file the event loop owes an association for.
    ///
    ///
    /// One slot: `Enter` on a second file replaces the first, which is what
    /// the key means.
    pub open: Option<crate::app::OpenRequest>,
    /// The file the execute prompt or the chooser is asking about,
    /// while one of them is on screen.
    ///
    /// The dialogs carry only what they draw, because
    /// [`crate::dialog::Dialog::handle_key`] is given a key and nothing else;
    /// the path waits here for the answer.
    pub subject: Option<VfsPath>,
    /// An external program the event loop owes the user (the design's
    /// `F4`), which will run with the terminal handed back to it.
    pub external: Option<crate::app::ExternalCommand>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Matcher, OpenHandler};

    /// A 64-bit ELF header, all 64 bytes of it.
    ///
    /// The whole header and not just the magic, because `infer`'s ELF matcher
    /// requires more than 52 bytes before it will answer - which is the reason
    /// [`HEAD_WINDOW`] exists and is documented.
    const ELF: &[u8] = b"\x7fELF\x02\x01\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\
                         \x02\x00\x3e\x00\x01\x00\x00\x00\x40\x10\x00\x00\x00\x00\x00\x00\
                         \x40\x00\x00\x00\x00\x00\x00\x00\x18\x20\x00\x00\x00\x00\x00\x00\
                         \x00\x00\x00\x00\x40\x00\x38\x00\x0b\x00\x40\x00\x1e\x00\x1d\x00";
    /// A JPEG's magic, the own worked example.
    const JPEG: &[u8] = b"\xff\xd8\xff\xe0\x00\x10JFIF";

    fn cfg(handlers: Vec<OpenHandler>) -> OpenConfig {
        OpenConfig {
            handlers,
            ..OpenConfig::default()
        }
    }

    fn handler(matcher: Matcher, command: &[&str]) -> OpenHandler {
        OpenHandler {
            matcher,
            command: command.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    #[test]
    fn a_txt_whose_bytes_are_a_jpeg_is_an_image() {
        // the own example, and invariant I13: "MIME type comes
        // from `mime_guess` on the extension, corrected by `infer` on the
        // content when the two disagree - a `.txt` that is actually a JPEG
        // opens as an image."
        assert_eq!(mime_of("holiday.txt", JPEG), "image/jpeg");
        // And the extension is still what answers when the bytes say nothing.
        assert_eq!(mime_of("notes.txt", b"plain words\n"), "text/plain");
        assert_eq!(mime_of("no-extension", b"plain words\n"), UNKNOWN_MIME);
    }

    #[test]
    fn the_prompt_names_what_the_file_actually_is() {
        // "ELF binary, shell script, Python script, detected
        // with `infer` from the content rather than guessed from the name".
        assert_eq!(kind_of(ELF, "deploy"), "ELF binary");
        assert_eq!(kind_of(b"#!/bin/sh\necho hi\n", "deploy"), "shell script");
        assert_eq!(
            kind_of(b"#!/usr/bin/env python3\nprint(1)\n", "deploy"),
            "Python script"
        );
        assert_eq!(kind_of(b"#!/usr/bin/perl\n", "x"), "Perl script");
        // No shebang and nothing `infer` knows: the name is the last word, and
        // `data` is the answer when even that says nothing.
        assert_eq!(kind_of(b"echo hi\n", "deploy.sh"), "shell script");
        assert_eq!(kind_of(b"\x01\x02\x00\x03", "blob"), "data");
        assert_eq!(kind_of(b"hello\n", "blob"), "text");
        assert_eq!(kind_of(JPEG, "holiday.txt"), "JPG image");
    }

    #[test]
    fn executability_is_the_mode_bits_and_nothing_else() {
        // Invariant I16, "A `.sh` without `+x` is data; a file
        // with no extension and `+x` is a program."
        assert!(is_executable(0o755));
        assert!(is_executable(0o100));
        assert!(!is_executable(0o644));
        assert!(!is_executable(0o000));
    }

    #[test]
    fn a_handler_beats_the_desktop_and_the_first_match_wins() {
        // the order, and "the TC model, where your own rules beat
        // the desktop's".
        let config = cfg(vec![
            handler(
                Matcher {
                    ext: vec!["png".into(), "jpg".into()],
                    ..Matcher::default()
                },
                &["imv", "{file}"],
            ),
            handler(
                Matcher {
                    mime: Some("image/*".into()),
                    ..Matcher::default()
                },
                &["never", "{file}"],
            ),
        ]);
        let path = VfsPath::local("/srv/media/a.png");
        match resolve(&config, &path, b"", 0o644) {
            Association::Handler(cmd) => {
                assert_eq!(cmd.program, "imv");
                assert_eq!(cmd.args, vec!["/srv/media/a.png".to_string()]);
            }
            other => panic!("expected the first handler, got {other:?}"),
        }
    }

    #[test]
    fn a_combination_matcher_means_all_of_it() {
        // "a combination means all of it must
        // hold."
        let config = cfg(vec![handler(
            Matcher {
                ext: vec!["sh".into()],
                mode: Some(ModeMatch::Exec),
                ..Matcher::default()
            },
            &["run-it", "{file}"],
        )]);
        let path = VfsPath::local("/srv/deploy.sh");
        assert!(matches!(
            resolve(&config, &path, b"", 0o755),
            Association::Handler(_)
        ));
        // The same name without `+x` falls through to the desktop.
        assert!(matches!(
            resolve(&config, &path, b"", 0o644),
            Association::Desktop(_)
        ));
    }

    #[test]
    fn an_empty_matcher_claims_nothing() {
        let config = cfg(vec![handler(Matcher::default(), &["never"])]);
        let path = VfsPath::local("/srv/a.png");
        assert!(matches!(
            resolve(&config, &path, b"", 0o644),
            Association::Desktop(_)
        ));
    }

    #[test]
    fn a_handler_with_no_command_is_inert_and_never_runs_the_file() {
        // a bad value in a config file is never fatal. And the
        // failure this guards against is specific: `expand_command` appends the
        // path when the template forgot it, so an empty `command` would make
        // the file itself argv[0].
        let config = cfg(vec![handler(
            Matcher {
                ext: vec!["png".into()],
                ..Matcher::default()
            },
            &[],
        )]);
        let path = VfsPath::local("/srv/a.png");
        assert!(matches!(
            resolve(&config, &path, b"", 0o644),
            Association::Desktop(_)
        ));
    }

    #[test]
    fn a_file_with_no_local_path_resolves_to_the_internal_viewer() {
        // step 3: "so `Enter` on an unknown type shows the file
        // rather than doing nothing." An archive member has no path the
        // desktop could be handed.
        let inside =
            VfsPath::local("/srv/a.zip").with_segment(crate::vfs::BackendKind::Archive, "/a.txt");
        assert!(matches!(
            resolve(&cfg(Vec::new()), &inside, b"", 0o644),
            Association::Viewer(_)
        ));
    }

    #[test]
    fn a_handler_expands_the_file_and_the_environment_and_no_shell() {
        // the second example is `command = ["$EDITOR", "{file}"]`.
        // `$HOME` is read rather than set,
        // because `std::env::set_var` is `unsafe` in edition 2024 and a test
        // that mutates the process environment races every other test in the
        // binary.
        let home = std::env::var("HOME").unwrap_or_default();
        let template: Vec<String> = ["-o", "$HOME/x", "${HOME}", "$", "$1", "{file}"]
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        let out = expand_command(&template, Path::new("/tmp/a b.txt"));
        assert_eq!(
            out,
            vec![
                "-o".to_string(),
                format!("{home}/x"),
                home.clone(),
                "$".to_string(),
                "$1".to_string(),
                "/tmp/a b.txt".to_string(),
            ]
        );
        // A word is expanded once: a variable whose value contains a `$` or a
        // `{file}` is not re-scanned, which is the difference between
        // substitution and a shell.
        assert_eq!(
            expand_command(&["{file}".to_string()], Path::new("/tmp/${HOME}")),
            vec!["/tmp/${HOME}".to_string()]
        );
        // And an unset variable is nothing, as a shell would have it.
        assert_eq!(
            expand_command(
                &["$HCMD_DEFINITELY_UNSET_0".to_string()],
                Path::new("/tmp/a")
            ),
            vec![String::new(), "/tmp/a".to_string()],
            "and the file is appended when the template forgot it"
        );
    }

    #[test]
    fn a_shebang_is_exec_d_and_a_script_without_one_goes_through_the_shell() {
        // and invariant I18: the file is a **single argument**
        // and no command string is ever built.
        let path = VfsPath::local("/srv/deploy.sh");
        assert_eq!(
            execute_argv(&path, b"#!/bin/sh\n", Some(OsStr::new("/bin/zsh"))),
            Ok(vec!["/srv/deploy.sh".to_string()])
        );
        assert_eq!(
            execute_argv(&path, b"echo hi\n", Some(OsStr::new("/bin/zsh"))),
            Ok(vec!["/bin/zsh".to_string(), "/srv/deploy.sh".to_string()])
        );
        assert_eq!(
            execute_argv(&path, b"echo hi\n", None),
            Ok(vec![
                FALLBACK_SHELL.to_string(),
                "/srv/deploy.sh".to_string()
            ]),
            "an unset $SHELL is /bin/sh and not a refusal"
        );
        // An ELF has no shebang either, and must not be handed to a shell.
        assert_eq!(
            execute_argv(&path, ELF, Some(OsStr::new("/bin/zsh"))),
            Ok(vec!["/srv/deploy.sh".to_string()])
        );
    }

    #[test]
    fn executing_from_a_non_local_path_is_refused_and_names_f5() {
        // Invariant I17, "The message says to copy it out with
        // `F5` first", and no temporary copy is made anywhere.
        let inside = VfsPath::local("/srv/a.zip")
            .with_segment(crate::vfs::BackendKind::Archive, "/deploy.sh");
        let err = execute_argv(&inside, b"#!/bin/sh\n", None).expect_err("refused");
        assert!(err.contains("F5"), "{err}");
    }

    #[test]
    fn a_mime_pattern_may_end_in_a_star() {
        assert!(mime_matches("text/*", "text/plain"));
        assert!(mime_matches("text/plain", "TEXT/PLAIN"));
        assert!(mime_matches("*", "anything/at-all"));
        assert!(!mime_matches("text/*", "image/png"));
        assert!(!mime_matches("", "text/plain"));
    }

    #[test]
    fn an_extension_is_never_the_leading_dot_of_a_hidden_file() {
        assert_eq!(extension_of("a.png"), "png");
        assert_eq!(extension_of(".bashrc"), "");
        assert_eq!(extension_of("archive.tar.gz"), "gz");
        assert_eq!(extension_of("plain"), "");
    }

    #[test]
    fn open_with_appends_the_path_to_the_stripped_exec() {
        // the field codes are already gone, so
        // the path is appended.
        let app = DesktopApp {
            name: "Image Viewer".to_string(),
            id: "org.example.imv".to_string(),
            exec: vec!["imv".to_string(), "-f".to_string()],
        };
        assert_eq!(
            open_with_argv(&app, Path::new("/srv/a.png")),
            vec![
                "imv".to_string(),
                "-f".to_string(),
                "/srv/a.png".to_string()
            ]
        );
    }

    #[test]
    fn the_desktop_entry_search_path_is_the_xdg_one() {
        // Read rather than set, for the `unsafe` reason above. Whatever the
        // environment says there is always at least one directory - the rule
        // has a hard-coded tail - and every one of them is absolute and ends
        // in `applications`.
        let dirs = desktop_entry_dirs();
        assert!(
            !dirs.is_empty(),
            "the XDG rule always yields somewhere to look"
        );
        for dir in &dirs {
            assert!(dir.is_absolute(), "{}", dir.display());
            assert_eq!(
                dir.file_name().and_then(std::ffi::OsStr::to_str),
                Some("applications")
            );
        }
        // The system tail is what the specification hard-codes when
        // `XDG_DATA_DIRS` says nothing, and it is the half of the answer that
        // does not depend on this machine's environment.
        if std::env::var_os("XDG_DATA_DIRS").is_none_or(|v| v.is_empty()) {
            assert_eq!(
                dirs.iter().rev().take(2).collect::<Vec<_>>(),
                vec![
                    &PathBuf::from("/usr/share/applications"),
                    &PathBuf::from("/usr/local/share/applications"),
                ],
                "{dirs:?}"
            );
        }
    }

    /// The environment variable that puts a re-invoked test binary into the
    /// child half of the `.desktop` association test.
    const APPS_CHILD: &str = "HCMD_OPEN_DESKTOP_FIXTURE";

    /// What the child prints once it has asserted everything. The parent looks
    /// for this line, so a child that returned early - a renamed test, a
    /// mis-spelled variable - cannot pass for a child that ran.
    const APPS_RAN: &str = "the .desktop fixture was read in full";

    /// A private directory for one test.
    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hcmd-open-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch directory");
        dir
    }

    /// Write one `.desktop` file into an `applications` directory.
    fn desktop_file(dir: &Path, appid: &str, body: &str) {
        std::fs::create_dir_all(dir).expect("applications directory");
        std::fs::write(
            dir.join(format!("{appid}.desktop")),
            format!("[Desktop Entry]\nType=Application\n{body}"),
        )
        .expect("desktop entry");
    }

    /// The child half: read a fixture the parent laid out and assert every
    /// rule [`applications_for`] applies to it.
    ///
    /// `#[ignore]` so it never runs as part of the suite, and gated on the
    /// environment variable so an explicit `--ignored` run does not start it
    /// either - the fixture only exists when the parent made it. Setting the
    /// environment has to happen before the process starts, because
    /// `std::env::set_var` is `unsafe` and this crate forbids `unsafe`, so
    /// re-invoking the test binary is the only way to point
    /// [`desktop_entry_dirs`] at a directory a test controls.
    #[test]
    #[ignore = "the child half of the .desktop association test"]
    fn desktop_association_child() {
        if std::env::var_os(APPS_CHILD).is_none() {
            return;
        }

        let apps = applications_for("text/plain");
        assert_eq!(apps.len(), 2, "{apps:?}");
        let [first, second] = apps.as_slice() else {
            panic!("two applications, got {apps:?}");
        };
        // Sorted by name, case-insensitively, and the data home shadows the
        // system directory for a repeated appid.
        assert_eq!(first.name, "Home Editor");
        assert_eq!(first.id, "org.example.editor");
        assert_eq!(first.exec, vec!["homeedit".to_string()]);
        assert_eq!(second.name, "Zed");
        assert_eq!(second.id, "org.example.zed");
        assert_eq!(second.exec, vec!["zed".to_string()]);

        // `text/*` matched above; `image/png` must not, and its own
        // application must be found under its own type.
        let images = applications_for("image/png");
        assert_eq!(
            images.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(),
            vec!["org.example.imv"],
            "{images:?}"
        );

        // Nothing else in the fixture is listed for anything: `NoDisplay`,
        // `Hidden` and a missing `Exec` each drop an entry.
        for mime in ["text/plain", "image/png", "application/pdf", "*"] {
            for app in applications_for(mime) {
                assert!(
                    !matches!(
                        app.id.as_str(),
                        "org.example.hidden" | "org.example.deleted" | "org.example.noexec"
                    ),
                    "{} was listed for {mime}",
                    app.id
                );
            }
        }

        println!("{APPS_RAN}");
    }

    /// which applications a type is offered, in
    /// what order, and which entries never appear.
    ///
    /// The assertions are in [`desktop_association_child`], which runs in a
    /// second process with `XDG_DATA_HOME` and `XDG_DATA_DIRS` pointing at the
    /// fixture below. An in-process test could only call
    /// [`applications_for`] against whatever desktop this machine happens to
    /// have, where an empty list is a legitimate answer and therefore proves
    /// nothing.
    #[test]
    fn the_desktop_advertises_its_applications_for_a_type() {
        let root = scratch("desktop");
        let home = root.join("home").join("applications");
        let system = root.join("sys").join("applications");

        desktop_file(
            &home,
            "org.example.editor",
            "Name=Home Editor\nExec=homeedit %f\nMimeType=text/plain;\n",
        );
        // The same appid lower down the search path: shadowed, never listed.
        desktop_file(
            &system,
            "org.example.editor",
            "Name=System Editor\nExec=sysedit %f\nMimeType=text/plain;\n",
        );
        desktop_file(
            &system,
            "org.example.zed",
            "Name=Zed\nExec=zed %U\nMimeType=text/*;\n",
        );
        desktop_file(
            &system,
            "org.example.imv",
            "Name=Imv\nExec=imv %f\nMimeType=image/png;\n",
        );
        desktop_file(
            &system,
            "org.example.hidden",
            "Name=Hidden\nExec=hidden %f\nMimeType=text/plain;\nNoDisplay=true\n",
        );
        desktop_file(
            &system,
            "org.example.deleted",
            "Name=Deleted\nExec=deleted %f\nMimeType=text/plain;\nHidden=true\n",
        );
        desktop_file(
            &system,
            "org.example.noexec",
            "Name=No Exec\nMimeType=text/plain;\n",
        );

        let exe = std::env::current_exe().expect("the test binary");
        let out = std::process::Command::new(exe)
            .args([
                "--exact",
                "ops::open::tests::desktop_association_child",
                "--ignored",
                "--nocapture",
            ])
            .env(APPS_CHILD, "1")
            .env("XDG_DATA_HOME", root.join("home"))
            .env("XDG_DATA_DIRS", root.join("sys"))
            .output()
            .expect("run the child");
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(out.status.success(), "{text}");
        assert!(text.contains(APPS_RAN), "the child never ran: {text}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn spawning_nothing_is_an_error_and_not_a_panic() {
        let err = spawn_detached(&[], None).expect_err("empty argv");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }
}
