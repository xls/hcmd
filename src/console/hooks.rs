//! The shell snippet that makes the cwd sync possible.
//!
//! > **Shell → panel**: rely on **OSC 7** … which the shell emits on each
//! > prompt. Inject the snippet at startup for bash and zsh
//! > (`PROMPT_COMMAND` / `precmd`); document it for fish and others.
//!
//! Four sequences go in, not one. `OSC 7` is what the design asks for;
//! `OSC 133 ; A` and `OSC 133 ; B` bracket the prompt so that the design's
//! command line knows where the prompt ends and the user's input begins. See
//! [`crate::console::osc`] for why that split cannot be guessed from the screen.
//!
//! `OSC 133 ; C` is the fourth, and it is what the completion
//! indicator is made of: "a completion indicator in the key bar shows when a
//! background command has produced output". Without a mark saying a command has
//! *started*, output from a build and the shell redrawing its own prompt are
//! the same bytes, and an indicator that cannot tell them apart lights on every
//! `cd` this application writes and means nothing. It costs no hook of its own -
//! bash expands `PS0` after reading a command and before running it, which is
//! exactly the moment, and zsh has `preexec_functions`. Neither needs a `DEBUG`
//! trap, which would be the intrusive way to ask the same question.
//!
//! # How it is installed, and what that costs
//!
//! The snippet is **written to the PTY as the shell's first input line**. There
//! is no cleaner route that does not take something away from the user:
//! `--rcfile` and `ZDOTDIR` replace the configuration we are trying to preserve,
//! and an environment variable is overwritten by any `.bashrc` that assigns
//! `PROMPT_COMMAND` or `PS1` - which is most of them. Writing it as input runs
//! *after* the shell has read its own configuration, so it composes with
//! whatever the user has rather than replacing it: `PROMPT_COMMAND` is
//! prepended to, `precmd_functions` is appended to, and `PS1` is wrapped.
//!
//! The cost is one line in the shell's own history and one screen of echo. The
//! echo is dealt with - the snippet ends in `clear`, so the console starts
//! blank. The history entry is not, and is the reason `console.inject_hooks`
//! exists: set it to `false` and install the equivalent in your own rc file.
//!
//! # Shells this does not inject into (the "document it for fish
//! and others")
//!
//! the design asks for the snippet in bash and zsh and for *documentation*
//! everywhere else, and that is the shape here: [`snippet`] answers `None` for
//! anything else and **nothing is said about it, ever**. A shell that never
//! emits `OSC 7` is not a broken shell - `dash` has no prompt hook to put one
//! in - and a warning at every prompt would be worse than the missing feature.
//! What happens instead is a quiet degradation, in this order:
//!
//! | The shell emits | Panel → shell | Shell → panel | the indicator |
//! |---|---|---|---|
//! | `OSC 7`, `OSC 133 ; B` and `; C` | yes | yes | yes |
//! | `OSC 7` and `OSC 133 ; B` | yes | yes | no - nothing says a command started |
//! | `OSC 7` only (fish) | no - the input line cannot be read, so a `cd` might land on top of a half-typed command | yes | no |
//! | neither (`dash`, a container's `sh`) | no | no | no |
//!
//! **bash before 4.4 lands on the second row.** The command-start mark is
//! written from `PS0`, which bash gained in 4.4, so a shell older than that
//! syncs the directory and brackets the prompt but can never light the
//! completion indicator. This is not hypothetical: macOS still ships 3.2.57
//! as `/bin/bash`, and anyone whose login shell is the system bash there gets
//! everything except the indicator. It is left as a gap rather than papered
//! over with a `DEBUG` trap, which is the intrusive route this module already
//! declines to take for `OSC 133 ; C`. The default shell on macOS is zsh,
//! which has `preexec_functions` and is unaffected.
//!
//! **fish needs nothing for the cwd half**: it has emitted `OSC 7` on every
//! prompt since 3.0, so the panel follows it with no help from us. It gets no
//! snippet here because the marks are the only thing missing, and adding them
//! means replacing the user's `fish_prompt` function - which is theirs. Anyone
//! who wants the other half, and the history capture with it, adds this to
//! `~/.config/fish/config.fish` (fish 4 emits the marks itself; check before
//! adding a second set):
//!
//! ```fish
//! functions -q __hcmd_prompt; or functions -c fish_prompt __hcmd_prompt
//! function fish_prompt
//!     printf '\e]133;A\a'
//!     __hcmd_prompt
//!     printf '\e]133;B\a'
//! end
//! function __hcmd_preexec --on-event fish_preexec
//!     printf '\e]133;C\a'
//! end
//! ```
//!
//! For any other shell, the contract is the four sequences and not the way they
//! are produced - anything that can run a command before each prompt, and one
//! before each command, can emit them:
//!
//! ```text
//! ESC ] 7 ; file://<host>/<percent-encoded $PWD> BEL   before the prompt
//! ESC ] 133 ; A BEL                                    at the prompt's first cell
//! ESC ] 133 ; B BEL                                    at the input's first cell
//! ESC ] 133 ; C BEL                                    after a command is read,
//!                                                      before it runs
//! ```
//!
//! `; C` is the one that is *not* part of the prompt string, so it is the one
//! that needs no non-printing markers. `; D` is not required: the next `; A`
//! ends a command as reliably, and every shell draws a prompt afterwards.
//!
//! The two `OSC 133` marks must be inside the prompt string, and must be marked
//! as non-printing in whatever way the shell spells that (`\[`…`\]` in bash,
//! `%{`…`%}` in zsh), or the shell will count them as characters and misplace
//! its own cursor. `%` is the only byte the path must escape, as `%25`; the
//! decoder takes every other byte literally, valid UTF-8 or not. Control bytes
//! are the exception, and they are not the decoder's doing: a terminal's OSC
//! parser drops `TAB`, `LF`, `CR` and `ESC` inside a sequence, so a path
//! containing one has to arrive percent-encoded or it arrives wrong.

use std::path::Path;

/// Which shell we are talking to, as far as prompt hooks are concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shell {
    /// `bash`.
    Bash,
    /// `zsh`.
    Zsh,
    /// `fish`, which already emits `OSC 7` and needs no snippet from us.
    Fish,
    /// Anything else. Documented rather than injected.
    Other,
}

impl Shell {
    /// Identify a shell from the path it is being started from.
    ///
    /// The file name only: `/usr/bin/bash`, `/bin/bash` and a `bash` on
    /// `$PATH` are one shell, and a login shell spelled `-bash` is still bash.
    pub fn detect(program: &str) -> Self {
        let name = Path::new(program)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(program)
            .trim_start_matches('-');
        match name {
            "bash" => Self::Bash,
            "zsh" => Self::Zsh,
            "fish" => Self::Fish,
            _ => Self::Other,
        }
    }

    /// A short name for a status-line message.
    pub const fn id(&self) -> &'static str {
        match self {
            Self::Bash => "bash",
            Self::Zsh => "zsh",
            Self::Fish => "fish",
            Self::Other => "shell",
        }
    }
}

/// The line to write into the PTY, or `None` for a shell we do not inject into.
///
/// It is one line, and it ends without a terminator: the caller adds the
/// carriage return, because it is the caller that owns writing to the PTY.
///
/// The leading space is deliberate - under `HISTCONTROL=ignorespace` (bash) or
/// `setopt histignorespace` (zsh) it keeps the line out of the shell's history
/// entirely. It is not a guarantee, only the cheapest thing that helps.
pub fn snippet(shell: Shell) -> Option<&'static str> {
    match shell {
        // **The hook runs *last*, and re-wraps `PS1` every time.** starship,
        // oh-my-posh, powerlevel10k and every framework like them rebuild the
        // prompt string on each prompt, from their own `PROMPT_COMMAND` /
        // `precmd` hook - so a `PS1` wrapped once at startup is thrown away by
        // the next prompt and the marks vanish. Verified against a real bash
        // with starship: the marks were gone by the first prompt. Appending the
        // hook rather than prepending it, and re-wrapping under a guard that
        // makes it idempotent, is what survives.
        //
        // `PROMPT_COMMAND` is a *string* in every bash before 5.1 and may be an
        // **array** after it, and the two need opposite syntax - appending a
        // string to an array assigns to element 0 and silently drops the rest.
        // `declare -p` tells them apart, portably, once.
        //
        // **`BEL`, not `ESC \\`.** the design writes the `BEL` form itself, and
        // it is the one a shell can spell without a fight: `PS1` is parsed for
        // backslash escapes *inside* the `\\[`…`\\]` non-printing markers, and
        // `\\033\\\\\\]` - ESC, then an escaped backslash, then the end marker -
        // was verified against bash 5 to lose the backslash and emit an
        // unterminated OSC. `\\a` has no such ambiguity, and `vte` accepts both
        // terminators identically (which is why `osc.rs` never sees either).
        //
        // **`%` and the four control bytes are the whole of the encoding.**
        // `crate::console::osc::percent_decode` needs `%` itself escaped so a
        // path containing `%2` is not misread, and every ordinary byte survives
        // the round trip untouched - a space, a `;` (vte splits on it and the
        // decoder rejoins), an accented letter, or a byte that is not UTF-8 at
        // all. `TAB`, `LF`, `CR` and `ESC` are the exception, and not because
        // of the decoder: vte's OSC parser *drops* a C0 byte inside a sequence
        // and terminates on `ESC`, so a directory whose name contains one would
        // arrive silently shortened. Encoded here, it arrives whole.
        //
        // **No authority in the URL.** `file:///path`, never
        // `file://host/path`. This shell is one the program started on this
        // machine, so its name carries no information the reader does not
        // already have, and every way of obtaining it is a way to be wrong:
        // the decoder compares the emitted host against
        // [`crate::console::osc::hostname`], which reads `/proc` and
        // `/etc/hostname`. Neither exists on macOS, so the comparison was
        // against an empty string, every OSC 7 carrying a host was rejected as
        // foreign, and the panel silently stopped following the shell on every
        // Mac. An empty authority is accepted everywhere and cannot disagree
        // with anything.
        //
        // **The append is guarded on `__hcmd7` already being there.** Sourcing
        // the snippet twice - a hand-written `.bashrc` copy plus
        // `console.inject_hooks` - would otherwise run the hook twice per
        // prompt and grow `PROMPT_COMMAND` every time. `declare -p` prints the
        // value whichever shape it has, so one `case` covers both.
        Shell::Bash => Some(concat!(
            " __hcmd7(){ local p=${PWD//\\%/%25};",
            " p=${p//$'\\t'/%09}; p=${p//$'\\n'/%0A};",
            " p=${p//$'\\r'/%0D}; p=${p//$'\\e'/%1B};",
            " printf '\\033]7;file://%s\\a' \"$p\";",
            " case \"$PS1\" in '\\[\\033]133;A'*) ;;",
            " *) PS1='\\[\\033]133;A\\a\\]'\"$PS1\"'\\[\\033]133;B\\a\\]';; esac;",
            " case \"$PS0\" in $'\\033]133;C'*) ;;",
            " *) PS0=$'\\033]133;C\\a'\"$PS0\";; esac; };",
            " case \"$(declare -p PROMPT_COMMAND 2>/dev/null)\" in",
            " *__hcmd7*) ;;",
            " \"declare -a\"*) PROMPT_COMMAND+=(__hcmd7);;",
            " *) PROMPT_COMMAND=\"${PROMPT_COMMAND:+$PROMPT_COMMAND;}__hcmd7\";; esac;",
            " __hcmd7; clear"
        )),
        Shell::Zsh => Some(concat!(
            " __hcmd7(){ local p=${PWD//\\%/%25};",
            " p=${p//$'\\t'/%09}; p=${p//$'\\n'/%0A};",
            " p=${p//$'\\r'/%0D}; p=${p//$'\\e'/%1B};",
            " printf '\\033]7;file://%s\\a' \"$p\";",
            " case $PS1 in $'%{\\e]133;A'*) ;;",
            " *) PS1=$'%{\\e]133;A\\a%}'$PS1$'%{\\e]133;B\\a%}';; esac; };",
            " __hcmd7c(){ printf '\\033]133;C\\a'; };",
            " case \" ${precmd_functions[*]} \" in *\" __hcmd7 \"*) ;;",
            " *) precmd_functions+=(__hcmd7);; esac;",
            " case \" ${preexec_functions[*]} \" in *\" __hcmd7c \"*) ;;",
            " *) preexec_functions+=(__hcmd7c);; esac;",
            " __hcmd7; clear"
        )),
        // fish emits OSC 7 on every prompt already (the "document it
        // for fish"), so the panel follows it with no help from us.
        Shell::Fish | Shell::Other => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn shells_are_identified_by_file_name() {
        assert_eq!(Shell::detect("/usr/bin/bash"), Shell::Bash);
        assert_eq!(Shell::detect("/bin/zsh"), Shell::Zsh);
        assert_eq!(Shell::detect("fish"), Shell::Fish);
        // A login shell is spelled with a leading dash in argv[0]; the path we
        // are given can carry it too.
        assert_eq!(Shell::detect("-bash"), Shell::Bash);
        assert_eq!(Shell::detect("/bin/dash"), Shell::Other);
        assert_eq!(Shell::detect("/usr/local/bin/nu"), Shell::Other);
    }

    #[test]
    fn the_snippet_carries_all_three_sequences() {
        for shell in [Shell::Bash, Shell::Zsh] {
            let text = snippet(shell).unwrap_or_default();
            assert!(text.contains("]7;file://"), "{shell:?} has no OSC 7");
            assert!(text.contains("]133;A"), "{shell:?} has no prompt mark");
            assert!(text.contains("]133;B"), "{shell:?} has no input mark");
            // Every sequence is BEL-terminated - see the comment on `snippet`;
            // an `ESC \\` inside a `PS1` does not survive bash's own parsing.
            assert!(
                !text.contains("\\033\\\\") && !text.contains("\\e\\\\"),
                "{shell:?} still spells a terminator as ESC backslash"
            );
            assert!(
                text.starts_with(' '),
                "{shell:?} does not open with the history-ignoring space"
            );
            assert!(
                !text.contains('\n') && !text.contains('\r'),
                "{shell:?} is not a single line"
            );
            // `%` is the one byte the decoder needs escaped, and the `%` in
            // the *pattern* must itself be escaped: zsh reads a leading `%`
            // there as an anchor to the end of the string, so the unescaped
            // spelling appended `%25` to every path and escaped nothing.
            assert!(
                text.contains("${PWD//\\%/%25}"),
                "{shell:?} does not escape %, or anchors the pattern in zsh"
            );
        }
        assert_eq!(snippet(Shell::Fish), None);
        assert_eq!(snippet(Shell::Other), None);
    }

    #[test]
    fn the_bytes_a_terminal_would_swallow_are_encoded() {
        // vte drops a C0 byte inside an OSC and terminates on `ESC`, so a path
        // containing one arrives shortened unless the shell encodes it.
        for shell in [Shell::Bash, Shell::Zsh] {
            let text = snippet(shell).unwrap_or_default();
            for (byte, code) in [
                ("\\t", "%09"),
                ("\\n", "%0A"),
                ("\\r", "%0D"),
                ("\\e", "%1B"),
            ] {
                assert!(
                    text.contains(&format!("$'{byte}'/{code}")),
                    "{shell:?} does not encode {byte} as {code}"
                );
            }
        }
    }

    #[test]
    fn the_hook_is_not_installed_twice() {
        // Sourcing it twice - a hand-written rc copy plus `inject_hooks` -
        // must not run the hook twice per prompt.
        assert!(
            snippet(Shell::Bash)
                .unwrap_or_default()
                .contains("*__hcmd7*)"),
            "bash appends without checking"
        );
        assert!(
            snippet(Shell::Zsh)
                .unwrap_or_default()
                .contains("*\" __hcmd7 \"*)"),
            "zsh appends without checking"
        );
    }

    // ------------------------------------------------------ a real shell ----

    /// The snippet, run by the shell it is written for, in a directory whose
    /// name exercises every byte the encoding has to survive.
    ///
    /// This is the test that would have caught each of the three things the
    /// comment on [`snippet`] says were found the hard way, and it needs no PTY:
    /// what is asserted is what the shell *printed* and what it left in the
    /// user's own variables.
    ///
    /// Skipped where the shell is not installed - a machine without `zsh` is not
    /// a failing machine.
    /// Both shells, an unremarkable directory, decoded byte for byte.
    #[test]
    fn an_ordinary_directory_round_trips_through_every_shell_we_inject_into() {
        let mut ran = 0;
        for (name, no_rc, shell) in [("bash", "--norc", Shell::Bash), ("zsh", "-f", Shell::Zsh)] {
            let Some(program) = shell_at(name) else {
                continue;
            };
            ran += 1;
            let tree = TempTree::new(&format!("plain-{name}"));
            let cwd = tree.plain_dir();
            let out = run(&program, no_rc, &cwd, shell, "true", "true");
            assert_eq!(
                decoded_cwd(&out),
                Some(cwd),
                "{name} did not report the directory it was in: {:?}",
                String::from_utf8_lossy(&out)
            );
        }
        assert!(
            ran > 0,
            "neither bash nor zsh is installed, so this proved nothing"
        );
    }

    #[test]
    fn bash_composes_with_what_the_user_had_and_emits_a_decodable_osc_7() {
        let Some(bash) = shell_at("bash") else {
            eprintln!("no bash on this machine; skipping");
            return;
        };
        let tree = TempTree::new("bash");
        let cwd = tree.awkward_dir();
        let out = run(
            &bash,
            "--norc",
            &cwd,
            Shell::Bash,
            "PROMPT_COMMAND='echo MINE'; PS1='> '",
            "printf 'PC=%s\\n' \"$PROMPT_COMMAND\"; printf 'PS1=%s\\n' \"$PS1\"",
        );
        let text = String::from_utf8_lossy(&out);

        assert!(
            text.contains("PC=echo MINE;__hcmd7"),
            "the user's PROMPT_COMMAND was clobbered: {text:?}"
        );
        assert_eq!(
            text.matches("__hcmd7").count(),
            1,
            "the hook was installed twice: {text:?}"
        );
        assert_eq!(
            text.matches("133;A").count(),
            1,
            "PS1 was wrapped twice: {text:?}"
        );
        assert_eq!(
            decoded_cwd(&out),
            Some(cwd),
            "the OSC 7 did not decode back to the directory bash was in"
        );
    }

    #[test]
    fn zsh_composes_with_what_the_user_had_and_emits_a_decodable_osc_7() {
        let Some(zsh) = shell_at("zsh") else {
            eprintln!("no zsh on this machine; skipping");
            return;
        };
        let tree = TempTree::new("zsh");
        let cwd = tree.awkward_dir();
        let out = run(
            &zsh,
            "-f",
            &cwd,
            Shell::Zsh,
            "precmd_functions=(mine); PS1='> '",
            "print -r -- \"PF=$precmd_functions\"; print -r -- \"PS1=$PS1\"",
        );
        let text = String::from_utf8_lossy(&out);

        assert!(
            text.contains("PF=mine __hcmd7"),
            "the user's precmd_functions were clobbered: {text:?}"
        );
        assert_eq!(
            text.matches("__hcmd7").count(),
            1,
            "the hook was installed twice: {text:?}"
        );
        assert_eq!(
            text.matches("133;A").count(),
            1,
            "PS1 was wrapped twice: {text:?}"
        );
        assert_eq!(
            decoded_cwd(&out),
            Some(cwd),
            "the OSC 7 did not decode back to the directory zsh was in"
        );
    }

    /// Run the snippet twice under `program`, in `cwd`, and return everything it
    /// wrote.
    ///
    /// Twice, because installing it twice is exactly what the guards are for.
    /// The trailing `clear` is dropped: there is no terminal here, and its
    /// escape sequences would be in the middle of what is asserted on.
    fn run(
        program: &str,
        no_rc: &str,
        cwd: &Path,
        shell: Shell,
        before: &str,
        after: &str,
    ) -> Vec<u8> {
        let whole = snippet(shell).unwrap_or_default();
        let body = whole
            .trim_end()
            .strip_suffix("clear")
            .unwrap_or(whole)
            .trim_end()
            .strip_suffix(';')
            .unwrap_or(whole);
        let script = format!("{before}; {body}; {body}; {after}");
        let out = std::process::Command::new(program)
            .arg(no_rc)
            .arg("-c")
            .arg(&script)
            .current_dir(cwd)
            .output()
            .expect("the shell runs");
        assert!(
            out.status.success(),
            "{program} refused the snippet: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        out.stdout
    }

    /// The directory named by the first `OSC 7` in `out`.
    ///
    /// Through the real decoder - [`crate::console::osc::decode_file_url`] -
    /// because "the shell printed something" is not the claim; "the shell
    /// printed something this application reads back as the same directory" is.
    fn decoded_cwd(out: &[u8]) -> Option<PathBuf> {
        let start = out
            .windows(4)
            .position(|w| w == b"\x1b]7;")?
            .saturating_add(4);
        let rest = out.get(start..)?;
        let end = rest.iter().position(|b| *b == 0x07)?;
        let payload = rest.get(..end)?;
        crate::console::osc::decode_file_url(payload, &crate::console::osc::hostname()).ok()
    }

    /// `name` in the usual places, or `None`.
    /// Find a shell to drive, `PATH` included.
    ///
    /// The three standard directories are not enough. A machine whose only
    /// `zsh` is one the developer built into `~/.local/bin` was reported as
    /// having no zsh at all, so the test skipped, printed nothing a test
    /// runner shows, and passed. That is the worst outcome available: for
    /// months the zsh snippet was exercised only on the macOS runner, and its
    /// first honest report was a red CI run.
    fn shell_at(name: &str) -> Option<String> {
        let from_path = std::env::var_os("PATH")
            .map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
            .unwrap_or_default();
        ["/bin", "/usr/bin", "/usr/local/bin"]
            .into_iter()
            .map(PathBuf::from)
            .chain(from_path)
            .map(|dir| dir.join(name))
            .find(|path| path.exists())
            .map(|path| path.to_string_lossy().into_owned())
    }

    /// A throwaway directory, removed on drop. Built by hand - `tempfile` is not
    /// on the dependency table.
    struct TempTree {
        root: PathBuf,
    }

    impl TempTree {
        fn new(tag: &str) -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "hcmd-hooks-{tag}-{pid}-{n}",
                pid = std::process::id()
            ));
            std::fs::create_dir_all(&root).expect("create temp tree");
            Self { root }
        }

        /// A subdirectory whose name holds every byte the encoding has to carry:
        /// a `%`, a space, a `;` (which `vte` splits an OSC on), a `#`, and a
        /// TAB (which it drops).
        /// A name with nothing special in it at all.
        ///
        /// The awkward name below is the interesting case, but it is not the
        /// common one, and for months it was the only one tested. zsh treats a
        /// `%` at the head of a substitution pattern as an anchor to the end
        /// of the string, so `${PWD//%/%25}` appended `%25` to *every* path
        /// and escaped nothing. Every ordinary directory was reported wrong
        /// and no test looked at an ordinary directory.
        fn plain_dir(&self) -> PathBuf {
            let dir = self.root.join("ordinary");
            std::fs::create_dir_all(&dir).expect("create temp dir");
            dir.canonicalize().unwrap_or(dir)
        }

        fn awkward_dir(&self) -> PathBuf {
            let dir = self.root.join("50% off; a\tb #1");
            std::fs::create_dir_all(&dir).expect("create temp dir");
            dir.canonicalize().unwrap_or(dir)
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
}
