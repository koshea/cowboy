//! The host side of network policy: routing `ask` questions to a UI.
//!
//! This is what replaced the token-authenticated TCP control channel. That channel
//! existed only because the policy engine ran *inside* the sandbox and therefore
//! could not be trusted with the decision, so each question had to be shipped out
//! over the network and a verdict shipped back — authenticated, because the agent
//! shared the bridge and could reach the port.
//!
//! With the engine running in the worker, asking is a channel send. Deleted with the
//! transport: the listener, its per-session token, the constant-time token
//! comparison guarding it, the bind-address logic that had to avoid `0.0.0.0`, and
//! the policy JSON file written to disk for the container to read.

use cowboy_core::netproto::{ApprovalScope, NetworkAttempt, Verdict};
use tokio::sync::{mpsc, oneshot};

/// A pending question for the user.
///
/// Carries its own reply channel, so the engine awaits exactly its own answer even
/// with several questions outstanding.
pub struct ApprovalRequest {
    pub attempt: NetworkAttempt,
    /// Why we are asking — for example that a DNS query looks like tunnelling — so
    /// the prompt can say more than the destination alone.
    pub reason: Option<String>,
    pub reply: oneshot::Sender<(Verdict, ApprovalScope)>,
}

/// A decision the policy made on its own, for the session log and activity view.
pub type NetworkEvent = (NetworkAttempt, Verdict, String);

/// An [`Approver`](cowboy_gateway::Approver) that forwards questions to a UI.
///
/// Fails closed in two distinct ways, both of which matter:
///
/// - the receiver is gone (no UI attached, or the session is shutting down), so the
///   send fails; and
/// - the receiver exists but never answers, because the user closed the modal or
///   detached — the reply channel is dropped.
///
/// Either way the answer is `Deny`. There is deliberately **no timeout that
/// allows**: a slow human must not become an open door.
pub struct ChannelApprover {
    approvals: mpsc::UnboundedSender<ApprovalRequest>,
    events: mpsc::UnboundedSender<NetworkEvent>,
}

impl ChannelApprover {
    pub fn new(
        approvals: mpsc::UnboundedSender<ApprovalRequest>,
        events: mpsc::UnboundedSender<NetworkEvent>,
    ) -> Self {
        Self { approvals, events }
    }
}

#[async_trait::async_trait]
impl cowboy_gateway::Approver for ChannelApprover {
    async fn ask(&self, attempt: &NetworkAttempt, reason: Option<&str>) -> Verdict {
        let (tx, rx) = oneshot::channel();
        let req = ApprovalRequest {
            attempt: attempt.clone(),
            reason: reason.map(String::from),
            reply: tx,
        };
        if self.approvals.send(req).is_err() {
            tracing::debug!(dest = %attempt.label(), "no approver attached; denying");
            return Verdict::Deny;
        }
        match rx.await {
            Ok((verdict, _scope)) => verdict,
            // The UI dropped the reply channel: treat as a refusal.
            Err(_) => Verdict::Deny,
        }
    }

    async fn event(&self, attempt: &NetworkAttempt, verdict: Verdict, reason: String) {
        let _ = self.events.send((attempt.clone(), verdict, reason));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cowboy_core::netproto::Protocol;
    use cowboy_gateway::Approver;

    fn attempt() -> NetworkAttempt {
        NetworkAttempt {
            protocol: Protocol::Tls,
            host: Some("example.com".into()),
            ip: None,
            port: 443,
            command_pid: Some(1234),
        }
    }

    #[tokio::test]
    async fn a_question_reaches_the_ui_and_its_answer_is_returned() {
        let (atx, mut arx) = mpsc::unbounded_channel();
        let (etx, _erx) = mpsc::unbounded_channel();
        let approver = ChannelApprover::new(atx, etx);

        let answering = tokio::spawn(async move {
            let req = arx.recv().await.expect("a question");
            assert_eq!(req.attempt.host.as_deref(), Some("example.com"));
            assert_eq!(
                req.attempt.command_pid,
                Some(1234),
                "the prompt should be able to name the command that asked"
            );
            let _ = req.reply.send((Verdict::Allow, ApprovalScope::Session));
        });

        assert_eq!(approver.ask(&attempt(), None).await, Verdict::Allow);
        answering.await.unwrap();
    }

    /// No UI attached: the send fails and the answer must be a refusal.
    #[tokio::test]
    async fn no_approver_attached_denies() {
        let (atx, arx) = mpsc::unbounded_channel();
        let (etx, _erx) = mpsc::unbounded_channel();
        drop(arx);
        let approver = ChannelApprover::new(atx, etx);
        assert_eq!(approver.ask(&attempt(), None).await, Verdict::Deny);
    }

    /// A UI that takes the question and then goes away (modal closed, client
    /// detached) must also deny — silence is not consent.
    #[tokio::test]
    async fn an_unanswered_question_denies() {
        let (atx, mut arx) = mpsc::unbounded_channel();
        let (etx, _erx) = mpsc::unbounded_channel();
        let approver = ChannelApprover::new(atx, etx);

        tokio::spawn(async move {
            let req = arx.recv().await.expect("a question");
            drop(req.reply); // detached without answering
        });

        assert_eq!(approver.ask(&attempt(), None).await, Verdict::Deny);
    }

    #[tokio::test]
    async fn reasons_are_passed_through_so_the_prompt_can_explain() {
        let (atx, mut arx) = mpsc::unbounded_channel();
        let (etx, _erx) = mpsc::unbounded_channel();
        let approver = ChannelApprover::new(atx, etx);

        tokio::spawn(async move {
            let req = arx.recv().await.unwrap();
            assert_eq!(req.reason.as_deref(), Some("dns tunnel suspected"));
            let _ = req.reply.send((Verdict::Deny, ApprovalScope::Once));
        });

        approver.ask(&attempt(), Some("dns tunnel suspected")).await;
    }

    #[tokio::test]
    async fn events_are_reported_without_blocking() {
        let (atx, _arx) = mpsc::unbounded_channel();
        let (etx, mut erx) = mpsc::unbounded_channel();
        let approver = ChannelApprover::new(atx, etx);

        approver
            .event(&attempt(), Verdict::Allow, "allow-listed".into())
            .await;

        let (a, v, why) = erx.try_recv().expect("an event");
        assert_eq!(a.host.as_deref(), Some("example.com"));
        assert_eq!(v, Verdict::Allow);
        assert_eq!(why, "allow-listed");
    }

    /// Concurrent questions must not get each other's answers.
    #[tokio::test]
    async fn concurrent_questions_get_their_own_answers() {
        let (atx, mut arx) = mpsc::unbounded_channel();
        let (etx, _erx) = mpsc::unbounded_channel();
        let approver = std::sync::Arc::new(ChannelApprover::new(atx, etx));

        tokio::spawn(async move {
            // Answer based on the port, in whatever order they arrive.
            while let Some(req) = arx.recv().await {
                let verdict = if req.attempt.port == 443 {
                    Verdict::Allow
                } else {
                    Verdict::Deny
                };
                let _ = req.reply.send((verdict, ApprovalScope::Once));
            }
        });

        let mut https = attempt();
        https.port = 443;
        let mut other = attempt();
        other.port = 9999;

        let a = approver.clone();
        let b = approver.clone();
        let (r1, r2) = tokio::join!(async move { a.ask(&https, None).await }, async move {
            b.ask(&other, None).await
        });
        assert_eq!(r1, Verdict::Allow);
        assert_eq!(r2, Verdict::Deny);
    }
}
