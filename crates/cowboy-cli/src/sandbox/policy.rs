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

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use cowboy_core::netproto::{ApprovalScope, NetworkAttempt, Verdict};
use cowboy_gateway::Answer;
use tokio::sync::{mpsc, oneshot};

/// What the agent loop can know about the network while it works: which approval
/// prompts are open right now, and what was refused or decided since it last
/// looked.
///
/// Display and pacing only. The verdict is the gateway's, and nothing here feeds
/// back into it: the loop uses this to tell the model why its request failed and
/// to wait for a human instead of retrying. That retrying is what this exists to
/// stop: a test that reconnects in a loop while a prompt sat unanswered was seen
/// producing 884 refused lookups in one session.
#[derive(Clone, Default)]
pub struct NetworkWatch {
    inner: Arc<WatchInner>,
}

#[derive(Default)]
struct WatchInner {
    state: Mutex<WatchState>,
    changed: tokio::sync::Notify,
}

#[derive(Default)]
struct WatchState {
    next: u64,
    /// Open prompts, `(id, destination)`.
    pending: Vec<(u64, String)>,
    /// What happened since the last [`NetworkWatch::take_notes`], aggregated by
    /// destination and outcome so a storm reads as one line with a count.
    notes: Vec<(String, u32)>,
}

/// Distinct note lines kept between two reads; the rest are counted, not listed.
const MAX_NOTES: usize = 8;

impl NetworkWatch {
    fn begin(&self, dest: String) -> u64 {
        let mut s = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        s.next += 1;
        let id = s.next;
        s.pending.push((id, dest));
        drop(s);
        self.inner.changed.notify_waiters();
        id
    }

    fn end(&self, id: u64) {
        let mut s = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        s.pending.retain(|(i, _)| *i != id);
        drop(s);
        self.inner.changed.notify_waiters();
    }

    fn note(&self, line: String) {
        let mut s = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((_, n)) = s.notes.iter_mut().find(|(l, _)| *l == line) {
            *n += 1;
        } else if s.notes.len() < MAX_NOTES {
            s.notes.push((line, 1));
        } else if let Some((_, n)) = s.notes.last_mut() {
            *n += 1;
        }
    }

    /// The destinations waiting on a human right now, deduplicated, in order.
    pub fn pending(&self) -> Vec<String> {
        let s = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut out: Vec<String> = Vec::new();
        for (_, d) in &s.pending {
            if !out.contains(d) {
                out.push(d.clone());
            }
        }
        out
    }

    /// What was refused or decided since the last call, one line per destination
    /// and outcome (`×N` when it repeated).
    pub fn take_notes(&self) -> Vec<String> {
        let mut s = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        std::mem::take(&mut s.notes)
            .into_iter()
            .map(|(l, n)| if n > 1 { format!("{l} (×{n})") } else { l })
            .collect()
    }

    /// Wait until no prompt is open, or `cancel` fires.
    pub async fn wait_clear(&self, cancel: &tokio_util::sync::CancellationToken) {
        loop {
            // Registered before checking, so a change between the check and the wait
            // is not missed.
            let changed = self.inner.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.pending().is_empty() {
                return;
            }
            tokio::select! {
                _ = &mut changed => {}
                _ = cancel.cancelled() => return,
            }
        }
    }
}

/// The note for a refusal the gateway made on its own.
fn refusal_note(attempt: &NetworkAttempt, reason: &str) -> String {
    format!("refused {} — {reason}", attempt.label())
}

/// The note for an answered prompt.
fn decision_note(attempt: &NetworkAttempt, verdict: Verdict, scope: ApprovalScope) -> String {
    let dest = attempt.label();
    match (verdict, scope) {
        (Verdict::Allow, ApprovalScope::Once) => {
            format!("the user allowed {dest} for that one connection")
        }
        (Verdict::Allow, _) => format!("the user allowed {dest}"),
        // Which of these it was is not visible here: a UI with nobody attached and a
        // subagent's auto-deny both answer exactly like a user's "deny".
        _ => format!(
            "{dest} was denied (by the user, by an unanswered prompt, or because nobody \
             can approve network access in this session); it stays blocked for the \
             session, so do not retry it or work around it — say that it is needed"
        ),
    }
}

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

/// A request waiting on the answer to an identical one already on screen.
type Waiter = oneshot::Sender<(Verdict, ApprovalScope)>;

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
    watch: NetworkWatch,
    /// Questions already on screen, by destination and reason, with the requests
    /// waiting on the same answer. A client that retries while the prompt is open
    /// would otherwise queue one identical prompt per attempt.
    in_flight: Mutex<HashMap<String, Vec<Waiter>>>,
}

impl ChannelApprover {
    pub fn new(
        approvals: mpsc::UnboundedSender<ApprovalRequest>,
        events: mpsc::UnboundedSender<NetworkEvent>,
    ) -> Self {
        Self {
            approvals,
            events,
            watch: NetworkWatch::default(),
            in_flight: Mutex::new(HashMap::new()),
        }
    }

    /// What this approver has open and has decided, for the agent loop.
    pub fn watch(&self) -> NetworkWatch {
        self.watch.clone()
    }

    /// Put one question to the UI and return the user's `(verdict, scope)`;
    /// `None` when there was nobody to ask or the UI dropped it.
    async fn ask_ui(
        &self,
        attempt: &NetworkAttempt,
        reason: Option<&str>,
    ) -> Option<(Verdict, ApprovalScope)> {
        let (tx, rx) = oneshot::channel();
        let req = ApprovalRequest {
            attempt: attempt.clone(),
            reason: reason.map(String::from),
            reply: tx,
        };
        if self.approvals.send(req).is_err() {
            tracing::debug!(dest = %attempt.label(), "no approver attached; denying");
            return None;
        }
        let id = self.watch.begin(attempt.label());
        let answer = rx.await.ok();
        self.watch.end(id);
        answer
    }
}

/// Whether the gateway may reuse a user's decision for later connections this
/// session (see [`ChannelApprover`]'s `answer`).
fn remembered(verdict: Verdict, scope: ApprovalScope) -> bool {
    !(verdict == Verdict::Allow && scope == ApprovalScope::Once)
}

#[async_trait::async_trait]
impl cowboy_gateway::Approver for ChannelApprover {
    async fn ask(&self, attempt: &NetworkAttempt, reason: Option<&str>) -> Verdict {
        self.answer(attempt, reason).await.verdict
    }

    /// The scope decides whether the gateway may reuse the verdict. Only an allow
    /// scoped `Once` is single-use; every deny stands for the session (a refusal that
    /// had to be repeated per connection would be a prompt storm, and re-asking is
    /// never the safer direction). Project/global allows are also persisted by the
    /// worker, which is what carries them past this session.
    ///
    /// A request identical to one already on screen (same destination and reason)
    /// waits for that answer instead of opening a second prompt. It shares the
    /// answer only when the answer stands for the session — every deny, and an
    /// allow wider than "once" — since "allow once" is one connection's.
    async fn answer(&self, attempt: &NetworkAttempt, reason: Option<&str>) -> Answer {
        let deny = Answer {
            verdict: Verdict::Deny,
            remember: true,
        };
        let key = format!("{}\u{1f}{}", attempt.label(), reason.unwrap_or(""));
        loop {
            let waiter = {
                let mut map = self.in_flight.lock().unwrap_or_else(|e| e.into_inner());
                match map.get_mut(&key) {
                    Some(waiters) => {
                        let (tx, rx) = oneshot::channel();
                        waiters.push(tx);
                        Some(rx)
                    }
                    None => {
                        map.insert(key.clone(), Vec::new());
                        None
                    }
                }
            };
            if let Some(rx) = waiter {
                match rx.await {
                    Ok((verdict, scope)) if remembered(verdict, scope) => {
                        return Answer {
                            verdict,
                            remember: true,
                        }
                    }
                    // "Allow once" was that connection's; ask for this one.
                    Ok(_) => continue,
                    Err(_) => return deny,
                }
            }
            let answer = self.ask_ui(attempt, reason).await;
            let waiters = self
                .in_flight
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&key)
                .unwrap_or_default();
            let (verdict, scope) = answer.unwrap_or((Verdict::Deny, ApprovalScope::Once));
            for w in waiters {
                let _ = w.send((verdict, scope));
            }
            self.watch.note(decision_note(attempt, verdict, scope));
            return match answer {
                Some((verdict, scope)) => Answer {
                    verdict,
                    remember: remembered(verdict, scope),
                },
                None => deny,
            };
        }
    }

    async fn event(&self, attempt: &NetworkAttempt, verdict: Verdict, reason: String) {
        if verdict == Verdict::Deny {
            self.watch.note(refusal_note(attempt, &reason));
        }
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

    /// The scope the user picked reaches the gateway: "allow once" is the only answer
    /// it must not reuse. It used to be discarded here, so "once — just this request"
    /// quietly lasted the whole session.
    #[tokio::test]
    async fn only_an_allow_once_is_single_use() {
        for (verdict, scope, remember) in [
            (Verdict::Allow, ApprovalScope::Once, false),
            (Verdict::Allow, ApprovalScope::Session, true),
            (Verdict::Allow, ApprovalScope::Project, true),
            (Verdict::Allow, ApprovalScope::Global, true),
            (Verdict::Deny, ApprovalScope::Once, true),
        ] {
            let (atx, mut arx) = mpsc::unbounded_channel();
            let (etx, _erx) = mpsc::unbounded_channel();
            let approver = ChannelApprover::new(atx, etx);
            tokio::spawn(async move {
                let req: ApprovalRequest = arx.recv().await.unwrap();
                let _ = req.reply.send((verdict, scope));
            });
            let answer = approver.answer(&attempt(), None).await;
            assert_eq!(
                answer,
                Answer { verdict, remember },
                "{verdict:?} {scope:?}"
            );
        }
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

#[cfg(test)]
mod watch_tests {
    use super::*;
    use cowboy_core::netproto::Protocol;
    use cowboy_gateway::Approver;

    fn attempt() -> NetworkAttempt {
        NetworkAttempt {
            protocol: Protocol::Tls,
            host: Some("example.com".into()),
            ip: None,
            port: 443,
            command_pid: None,
        }
    }

    fn approver() -> (
        Arc<ChannelApprover>,
        mpsc::UnboundedReceiver<ApprovalRequest>,
    ) {
        let (atx, arx) = mpsc::unbounded_channel();
        let (etx, _erx) = mpsc::unbounded_channel();
        (Arc::new(ChannelApprover::new(atx, etx)), arx)
    }

    fn ask(a: &Arc<ChannelApprover>) -> tokio::task::JoinHandle<Answer> {
        let a = a.clone();
        tokio::spawn(async move { a.answer(&attempt(), None).await })
    }

    /// A client retrying while the prompt is open waits on that prompt instead of
    /// queueing a duplicate, and the loop can see the prompt while it is open.
    #[tokio::test]
    async fn a_repeated_request_shares_the_open_prompt() {
        let (approver, mut arx) = approver();
        let watch = approver.watch();
        let first = ask(&approver);
        let req = arx.recv().await.expect("one prompt");
        let second = ask(&approver);
        tokio::task::yield_now().await;
        assert_eq!(watch.pending(), ["example.com:443".to_string()]);
        assert!(arx.try_recv().is_err(), "no duplicate prompt");
        let _ = req.reply.send((Verdict::Allow, ApprovalScope::Session));
        for h in [first, second] {
            assert_eq!(
                h.await.unwrap(),
                Answer {
                    verdict: Verdict::Allow,
                    remember: true
                }
            );
        }
        assert!(watch.pending().is_empty());
        watch
            .wait_clear(&tokio_util::sync::CancellationToken::new())
            .await;
        assert_eq!(
            watch.take_notes(),
            ["the user allowed example.com:443".to_string()]
        );
        assert!(watch.take_notes().is_empty(), "notes are taken once");
    }

    /// "Allow once" is one connection's: a request waiting on it asks for itself.
    #[tokio::test]
    async fn allow_once_is_not_shared() {
        let (approver, mut arx) = approver();
        let first = ask(&approver);
        let req = arx.recv().await.unwrap();
        let second = ask(&approver);
        tokio::task::yield_now().await;
        let _ = req.reply.send((Verdict::Allow, ApprovalScope::Once));
        assert!(!first.await.unwrap().remember);
        let again = arx.recv().await.expect("the waiter asks for itself");
        let _ = again.reply.send((Verdict::Deny, ApprovalScope::Once));
        assert_eq!(second.await.unwrap().verdict, Verdict::Deny);
    }

    /// The loop's pause ends when the prompt is answered.
    #[tokio::test]
    async fn waiting_ends_when_the_prompt_is_answered() {
        let (approver, mut arx) = approver();
        let watch = approver.watch();
        let pending = ask(&approver);
        let req = arx.recv().await.unwrap();
        let waiter = {
            let watch = watch.clone();
            tokio::spawn(async move {
                watch
                    .wait_clear(&tokio_util::sync::CancellationToken::new())
                    .await
            })
        };
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(!waiter.is_finished(), "waits while the prompt is open");
        let _ = req.reply.send((Verdict::Deny, ApprovalScope::Once));
        pending.await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), waiter)
            .await
            .expect("the wait ends")
            .unwrap();
        assert!(watch.take_notes()[0].contains("stays blocked"));
    }

    /// A gateway refusal storm reads as one line with a count.
    #[tokio::test]
    async fn refusals_are_aggregated_for_the_model() {
        let (approver, _arx) = approver();
        for _ in 0..3 {
            approver
                .event(&attempt(), Verdict::Deny, "policy".into())
                .await;
        }
        approver
            .event(&attempt(), Verdict::Allow, "ok".into())
            .await;
        assert_eq!(
            approver.watch().take_notes(),
            ["refused example.com:443 — policy (×3)".to_string()]
        );
    }
}
