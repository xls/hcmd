//! The only outbound HTTP in the program.
//!
//! Two questions are asked of GitHub and nothing else is: *is there a newer
//! release*, and *are there themes in the repository this machine does not
//! have*. Both are asked because the user did something that means to ask
//! them - opening the theme picker, or checking for an update - and neither
//! is asked on startup, in the background, or on a timer.
//!
//! # Why it is small
//!
//! `rustls` is already linked for SFTP and FTPS, so this adds a client over a
//! TLS stack that was already paid for. Everything here is one blocking `GET`
//! with a timeout, called from a worker thread. There is no connection pool,
//! no retry, and no cache: a request that fails is a question left
//! unanswered, and the feature that asked it carries on without an answer.
//!
//! # What a failure means
//!
//! Nothing, to the rest of the program. A machine with no network, a
//! corporate proxy, GitHub being down, or rate limiting all come back as an
//! error the caller reports and forgets. No feature is unavailable because
//! this failed; the theme picker still lists what is on disk and the version
//! check simply does not know.

use std::time::Duration;

use crate::error::{Error, Result};

/// The repository the two questions are about.
pub const REPO: &str = "xls/hcmd";

/// How long to wait before giving up on a question nobody asked twice.
///
/// Short on purpose. This is never on a path the user is waiting for, and a
/// request that hangs for a minute holds a worker thread for a minute.
const TIMEOUT: Duration = Duration::from_secs(10);

/// The biggest answer worth reading.
///
/// The release JSON is a few kilobytes and a theme is a few hundred lines.
/// Anything past this is not the thing that was asked for, and reading it
/// into memory would be doing an unbounded amount of work on the word of a
/// remote server.
const MAX_BODY: u64 = 2 * 1024 * 1024;

/// Identify the program, because the GitHub API refuses a request that does
/// not.
fn agent() -> String {
    format!("hcmd/{}", env!("CARGO_PKG_VERSION"))
}

/// `GET` a URL, with a timeout and a size limit.
///
/// The status is checked: a 404 or a 403 is an error here rather than a body
/// that fails to parse three layers away, so the message names what actually
/// happened.
pub fn get(url: &str) -> Result<Vec<u8>> {
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(TIMEOUT))
        .user_agent(agent())
        .build();
    let http = ureq::Agent::new_with_config(config);

    let mut response = http
        .get(url)
        .call()
        .map_err(|e| Error::msg(format!("{url}: {e}")))?;

    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(Error::msg(format!("{url}: the server answered {status}")));
    }

    response
        .body_mut()
        .with_config()
        .limit(MAX_BODY)
        .read_to_vec()
        .map_err(|e| Error::msg(format!("{url}: {e}")))
}

/// [`get`], as text.
pub fn get_text(url: &str) -> Result<String> {
    let bytes = get(url)?;
    String::from_utf8(bytes).map_err(|_| Error::msg(format!("{url}: the answer was not text")))
}
