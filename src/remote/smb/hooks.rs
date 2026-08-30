//! How a connect attempt asks the user a question.
//!
//! The connect task is async and long-lived and the dialogs are on the event
//! loop, so a question goes out as a [`RemoteEvent`] carrying a
//! `oneshot::Sender` and the answer comes back through it. **Dropping that
//! sender is a refusal**, which is how `Esc` cancels a connect with no extra
//! code path. This is the SMB twin of `sftp::ConnectHooks`, without the
//! host-key half: SMB has no host key to verify.

use crate::app::RemoteEvent;
use crate::dialog::SecretAnswer;
use crate::remote::auth::SecretKind;
use crate::remote::connect::ConnectId;

/// How a connect attempt asks the user for a password.
///
/// The connect task is async and the dialogs are on the event loop, so a
/// question goes out as a [`RemoteEvent`] carrying a `oneshot::Sender` and the
/// answer comes back through it. **Dropping that sender is a refusal**, which
/// is how `Esc` cancels a connect with no extra code path.
#[derive(Clone)]
pub struct SmbHooks {
    /// Where a question goes.
    events: tokio::sync::mpsc::Sender<RemoteEvent>,
    /// Which attempt is asking, so an answer to an abandoned attempt is
    /// dropped rather than applied.
    attempt: u64,
}

impl SmbHooks {
    /// The hooks for one attempt.
    pub fn new(events: tokio::sync::mpsc::Sender<RemoteEvent>, attempt: ConnectId) -> Self {
        Self {
            events,
            attempt: attempt.0,
        }
    }

    /// Ask for a password. `None` for every way of not answering.
    pub(crate) async fn ask_secret(
        &self,
        kind: SecretKind,
        offer_keyring: bool,
    ) -> Option<SecretAnswer> {
        let (reply, answer) = tokio::sync::oneshot::channel();
        let event = RemoteEvent::Secret {
            attempt: ConnectId(self.attempt),
            kind,
            offer_keyring,
            reply,
        };
        if self.events.send(event).await.is_err() {
            return None;
        }
        answer.await.ok().flatten()
    }
}

impl std::fmt::Debug for SmbHooks {
    /// The attempt, and nothing that could be walked to a pending secret.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SmbHooks")
            .field("attempt", &self.attempt)
            .finish()
    }
}
