//! `hcmd` - entry point, argument parsing, terminal setup and teardown.
//!
//! Everything the application does once the runtime exists lives in
//! [`holoscommander::runtime`], which is a library module so that the loop,
//! its channels and the tasks on the other end of them can be driven from a
//! test instead of only from a terminal.

use std::process::ExitCode;

use holoscommander::config;
use holoscommander::error::Result;
use holoscommander::runtime::event_loop;
use holoscommander::term::Term;
use holoscommander::{BIN_NAME, VERSION};

/// What the command line asked for.
#[derive(Debug, PartialEq, Eq)]
enum Mode {
    Run,
    KeyTest,
    CheckConfig,
    UpdateConfig,
    Version,
    Help,
    Unknown(String),
}

/// Argument parsing. Four flags do not justify a dependency (the design
/// rule 5).
///
/// The first recognised mode wins, so `--version --keytest` prints the version
/// rather than quietly doing the last thing on the line. Anything unrecognised -
/// flag or not, since no positional argument is defined - stops parsing and
/// becomes the usage error.
fn parse_args<I, S>(args: I) -> Mode
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut mode = Mode::Run;
    for arg in args {
        let arg = arg.as_ref();
        let found = match arg {
            "--keytest" => Mode::KeyTest,
            "--check-config" => Mode::CheckConfig,
            "--update-config" => Mode::UpdateConfig,
            "--version" | "-V" => Mode::Version,
            "--help" | "-h" => Mode::Help,
            other => return Mode::Unknown(other.to_string()),
        };
        if mode == Mode::Run {
            mode = found;
        }
    }
    mode
}

fn usage() -> String {
    format!(
        "{BIN_NAME} {VERSION} - a Total Commander alternative for the terminal\n\
         \n\
         usage: {BIN_NAME} [options]\n\
         \n\
         options:\n  \
         --keytest        show how this terminal encodes each key\n  \
         --check-config   validate the configuration files and exit\n  \
         --update-config  regenerate config.toml and keymap.toml, keeping your changes\n  \
         -V, --version    print the version\n  \
         -h, --help       print this message\n\
         \n\
         environment:\n  \
         HCMD_KEYBOARD_PROTOCOL   auto | enhanced | legacy; overrides\n                           \
         terminal.keyboard_protocol for a terminal that cannot\n                           \
         answer a capability query\n\
         \n\
         configuration lives in ~/.config/holoscommander/ and is created on first run."
    )
}

fn main() -> ExitCode {
    match parse_args(std::env::args().skip(1)) {
        Mode::Help => {
            println!("{}", usage());
            ExitCode::SUCCESS
        }
        Mode::Version => {
            println!("{BIN_NAME} {VERSION}");
            ExitCode::SUCCESS
        }
        Mode::Unknown(arg) => {
            eprintln!("{BIN_NAME}: unknown option {arg:?}\n\n{}", usage());
            ExitCode::from(2)
        }
        Mode::CheckConfig => {
            let code = config::check_config();
            ExitCode::from(u8::try_from(code).unwrap_or(1))
        }
        Mode::UpdateConfig => {
            let code = config::update_config();
            ExitCode::from(u8::try_from(code).unwrap_or(1))
        }
        Mode::KeyTest => {
            // The same panic hook as the application: --keytest puts the
            // terminal in raw mode, so it owes the same restore.
            Term::install_panic_hook();
            let result = Term::keytest();
            let _ = Term::restore();
            match result {
                Ok(()) => ExitCode::SUCCESS,
                Err(err) => {
                    eprintln!("{BIN_NAME}: {err}");
                    ExitCode::FAILURE
                }
            }
        }
        Mode::Run => match run() {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("{BIN_NAME}: {err}");
                ExitCode::FAILURE
            }
        },
    }
}

/// Build the runtime and drive the event loop, restoring the terminal on every
/// exit path.
fn run() -> Result<()> {
    // The panic hook goes in first, so a panic anywhere after this - including
    // inside terminal setup - leaves the terminal usable.
    Term::install_panic_hook();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    let result = runtime.block_on(event_loop());

    // Explicit as well as on Drop, so the terminal is back before any error is
    // printed.
    let _ = Term::restore();
    result
}

#[cfg(test)]
mod tests {
    use super::{Mode, parse_args, usage};

    #[test]
    fn no_arguments_runs_the_application() {
        assert_eq!(parse_args(Vec::<String>::new()), Mode::Run);
    }

    #[test]
    fn every_documented_flag_is_recognised() {
        assert_eq!(parse_args(["--keytest"]), Mode::KeyTest);
        assert_eq!(parse_args(["--check-config"]), Mode::CheckConfig);
        assert_eq!(parse_args(["--version"]), Mode::Version);
        assert_eq!(parse_args(["-V"]), Mode::Version);
        assert_eq!(parse_args(["--help"]), Mode::Help);
        assert_eq!(parse_args(["-h"]), Mode::Help);
    }

    #[test]
    fn an_unknown_flag_is_reported_by_name() {
        assert_eq!(
            parse_args(["--wibble"]),
            Mode::Unknown("--wibble".to_string())
        );
        // Not a flag at all: no positional argument is defined, so it is a
        // usage error rather than a silently ignored word.
        assert_eq!(parse_args(["/tmp"]), Mode::Unknown("/tmp".to_string()));
        assert_eq!(parse_args([""]), Mode::Unknown(String::new()));
    }

    #[test]
    fn an_unknown_flag_wins_over_a_valid_one_that_follows_it() {
        assert_eq!(
            parse_args(["--wibble", "--help"]),
            Mode::Unknown("--wibble".to_string())
        );
    }

    #[test]
    fn the_first_recognised_mode_wins() {
        assert_eq!(parse_args(["--version", "--keytest"]), Mode::Version);
        assert_eq!(parse_args(["--keytest", "--version"]), Mode::KeyTest);
    }

    #[test]
    fn a_repeated_flag_is_not_an_error() {
        assert_eq!(parse_args(["--help", "--help"]), Mode::Help);
    }

    #[test]
    fn the_usage_message_documents_every_flag_the_parser_accepts() {
        let text = usage();
        for flag in [
            "--keytest",
            "--check-config",
            "--version",
            "--help",
            "HCMD_KEYBOARD_PROTOCOL",
        ] {
            assert!(text.contains(flag), "usage does not mention {flag}");
        }
    }

    #[test]
    fn every_option_line_indents_two_and_describes_itself_in_one_column() {
        // A `\\` continuation in a Rust string literal eats the next line's
        // leading whitespace, so the indent has to be written before the
        // backslash. Two of these lines once had it after, which cost them a
        // space of indent and all of their column alignment.
        let text = usage();
        let options: Vec<&str> = text
            .lines()
            .skip_while(|l| *l != "options:")
            .skip(1)
            .take_while(|l| !l.is_empty())
            .collect();
        assert_eq!(
            options.len(),
            5,
            "expected five option lines, got {options:?}"
        );
        for line in &options {
            assert!(line.starts_with("  -"), "not indented by two: {line:?}");
            assert!(
                !line.starts_with("   "),
                "indented by more than two: {line:?}"
            );
        }
        // The description starts after the first run of two-or-more spaces that
        // follows the flags themselves, which is why the search skips the
        // indent: "-V, --version" has a single space inside it.
        let columns: Vec<usize> = options
            .iter()
            .filter_map(|l| {
                let gap = l[2..].find("  ")? + 2;
                Some(gap + l[gap..].len() - l[gap..].trim_start().len())
            })
            .collect();
        assert_eq!(
            columns.len(),
            options.len(),
            "a line has no description: {options:?}"
        );
        assert!(
            columns.windows(2).all(|w| w[0] == w[1]),
            "descriptions do not share one column: {columns:?} in {options:?}"
        );
    }
}
