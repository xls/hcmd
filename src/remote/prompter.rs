//! How a connect attempt asks the user for a secret.
//!
//! One question, asked from three protocols, and it used to be written three
//! times: SFTP's `ConnectHooks`, SMB's `SmbHooks` and FTP's `BlockingHooks`
//! each built the same [`RemoteEvent::Secret`], sent it down the same channel
//! and waited on the same `oneshot`. The only real difference between them was
//! that FTP runs on a blocking thread and the other two do not, which is a
//! difference in how the answer is awaited and not in what is being asked.
//!
//! **Dropping the reply channel is a refusal**, which is how `Esc` cancels a
//! connect with no second code path, and it is why every way of not answering
//! comes back as `None` rather than as an error to handle.

use std::fmt;

use tokio::sync::mpsc::Sender;

use crate::app::RemoteEvent;
use crate::dialog::SecretAnswer;
use crate::remote::auth::SecretKind;
use crate::remote::connect::ConnectId;

/// Where a question goes.
enum Ask {
    /// To the event loop, which is where the dialogs are.
    Loop {
        /// The channel the question rides.
        events: Sender<RemoteEvent>,
        /// Which attempt is asking, so an answer to an abandoned attempt is
        /// dropped rather than applied. Held as the `u64` inside
        /// [`ConnectId`] so this module does not depend on another's derives.
        attempt: u64,
    },
    /// To a closure. For a connect with nowhere to ask - a test, or any
    /// context with no UI - and the only way to answer without a screen.
    Fixed(std::sync::Arc<dyn Fn(SecretKind, bool) -> Option<SecretAnswer> + Send + Sync>),
}

impl Clone for Ask {
    fn clone(&self) -> Self {
        match self {
            Self::Loop { events, attempt } => Self::Loop {
                events: events.clone(),
                attempt: *attempt,
            },
            Self::Fixed(ask) => Self::Fixed(std::sync::Arc::clone(ask)),
        }
    }
}

/// How a connect attempt asks for a password or a passphrase.
#[derive(Clone)]
pub struct Prompter {
    ask: Ask,
}

impl Prompter {
    /// Ask the event loop, on behalf of one attempt.
    #[must_use]
    pub fn to_loop(events: Sender<RemoteEvent>, attempt: ConnectId) -> Self {
        Self {
            ask: Ask::Loop {
                events,
                attempt: attempt.0,
            },
        }
    }

    /// Answer from `ask` instead of from a screen.
    pub fn fixed(
        ask: impl Fn(SecretKind, bool) -> Option<SecretAnswer> + Send + Sync + 'static,
    ) -> Self {
        Self {
            ask: Ask::Fixed(std::sync::Arc::new(ask)),
        }
    }

    /// Refuse every question, for a connect that has nowhere to ask.
    #[must_use]
    pub fn refusing() -> Self {
        Self::fixed(|_, _| None)
    }

    /// Which attempt is asking, where there is one.
    #[must_use]
    pub const fn attempt(&self) -> Option<ConnectId> {
        match &self.ask {
            Ask::Loop { attempt, .. } => Some(ConnectId(*attempt)),
            Ask::Fixed(_) => None,
        }
    }

    /// The channel a question rides, for one a protocol asks on its own.
    ///
    /// SFTP's host key is the only one: no other protocol here has a key to
    /// verify, so it stays SFTP's question and only the way of sending it is
    /// shared. `None` where there is no event loop to ask.
    #[must_use]
    pub const fn events(&self) -> Option<&Sender<RemoteEvent>> {
        match &self.ask {
            Ask::Loop { events, .. } => Some(events),
            Ask::Fixed(_) => None,
        }
    }

    /// Put the question on the screen and wait. `None` for every way of not
    /// answering: a cancel, a dismissed dialog, a dropped reply channel, or an
    /// event loop that has already gone.
    pub async fn ask_secret(&self, kind: SecretKind, offer_keyring: bool) -> Option<SecretAnswer> {
        match &self.ask {
            Ask::Loop { events, attempt } => {
                let (reply, answer) = tokio::sync::oneshot::channel();
                let event = RemoteEvent::Secret {
                    attempt: ConnectId(*attempt),
                    kind,
                    offer_keyring,
                    reply,
                };
                if events.send(event).await.is_err() {
                    return None;
                }
                answer.await.ok().flatten()
            }
            Ask::Fixed(ask) => ask(kind, offer_keyring),
        }
    }

    /// The same question from a thread that cannot await, which is where FTP
    /// runs. The protocol is identical; only the waiting differs.
    pub fn ask_secret_blocking(
        &self,
        kind: SecretKind,
        offer_keyring: bool,
    ) -> Option<SecretAnswer> {
        match &self.ask {
            Ask::Loop { events, attempt } => {
                let (reply, answer) = tokio::sync::oneshot::channel();
                let event = RemoteEvent::Secret {
                    attempt: ConnectId(*attempt),
                    kind,
                    offer_keyring,
                    reply,
                };
                if events.blocking_send(event).is_err() {
                    return None;
                }
                answer.blocking_recv().ok().flatten()
            }
            Ask::Fixed(ask) => ask(kind, offer_keyring),
        }
    }
}

impl fmt::Debug for Prompter {
    /// The attempt, and nothing that could be walked to a pending secret.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Prompter")
            .field("attempt", &self.attempt())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::secret::Secret;

    fn answer() -> SecretAnswer {
        SecretAnswer {
            secret: Secret::from_str("hunter2"),
            remember: false,
        }
    }

    fn a_password() -> SecretKind {
        SecretKind::Password {
            authority: "t@h".to_string(),
        }
    }

    #[tokio::test]
    async fn the_question_reaches_the_event_loop_carrying_its_attempt() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let prompter = Prompter::to_loop(tx, ConnectId(7));
        assert_eq!(prompter.attempt(), Some(ConnectId(7)));

        let asking = tokio::spawn(async move { prompter.ask_secret(a_password(), true).await });
        let Some(RemoteEvent::Secret {
            attempt,
            offer_keyring,
            reply,
            ..
        }) = rx.recv().await
        else {
            panic!("the question did not arrive");
        };
        assert_eq!(attempt, ConnectId(7), "and says which attempt is asking");
        assert!(offer_keyring);
        let _ = reply.send(Some(answer()));
        assert!(
            asking.await.expect("joined").is_some(),
            "the answer comes back"
        );
    }

    #[tokio::test]
    async fn a_dropped_reply_is_a_refusal_and_so_is_a_loop_that_has_gone() {
        // How `Esc` cancels a connect, with no second code path for it.
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let prompter = Prompter::to_loop(tx, ConnectId(1));
        let asking = tokio::spawn(async move { prompter.ask_secret(a_password(), false).await });
        let event = rx.recv().await.expect("the question");
        drop(event);
        assert!(
            asking.await.expect("joined").is_none(),
            "a dropped reply refuses"
        );

        // And with nobody listening at all.
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        drop(rx);
        let gone = Prompter::to_loop(tx, ConnectId(2));
        assert!(gone.ask_secret(a_password(), false).await.is_none());
    }

    #[test]
    fn a_prompter_with_nowhere_to_ask_answers_from_its_closure() {
        // The shape a test or a headless connect uses, and the reason this is
        // one type rather than one per protocol.
        assert!(
            Prompter::refusing()
                .ask_secret_blocking(a_password(), false)
                .is_none()
        );
        let fixed = Prompter::fixed(|_, _| Some(answer()));
        assert!(fixed.ask_secret_blocking(a_password(), true).is_some());
        assert_eq!(fixed.attempt(), None, "there is no attempt behind it");
    }

    #[test]
    fn nothing_that_could_reach_a_secret_is_printed() {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let shown = format!("{:?}", Prompter::to_loop(tx, ConnectId(3)));
        assert!(shown.contains("attempt"), "{shown}");
        assert!(!shown.contains("Sender"), "never the channel: {shown}");
    }
}
