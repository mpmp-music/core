//! Confirmations an agent waits for (`wait_for_confirmation`).
//!
//! Core tells agents only the token (`needs_confirmation`), and every subscriber the answer
//! (`ConfirmationResolved`, without who asked). So the adapter keeps, per session, the tokens
//! core gave that session: only those can be waited for, and only by that session. Answers
//! are recorded as they come (one subscription for the whole adapter, opened before any
//! connection), also for tokens not claimed yet: an answer may arrive before the tool call
//! that got the token has recorded it.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use mpmp_core::contract::{CommandResult, ConfirmationOutcome, ConfirmationReason};
use tokio::sync::watch;

/// Answers kept for tokens no session has claimed (yet), and for how long.
const UNCLAIMED: usize = 256;

const UNCLAIMED_FOR: Duration = Duration::from_secs(15 * 60);

/// Tokens kept per session; the oldest answered ones go first.
const PER_SESSION: usize = 256;

/// One connection of one agent.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct Owner {
    pub agent: String,
    pub session: String,
}

/// How a confirmation ended, or that it has not.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Status {
    /// The user approved; the command ran with this result.
    Approved(Box<CommandResult>),
    Rejected,
    Expired,
    /// The request went away unanswered (its session ended, its agent was removed).
    Withdrawn,
    /// Not approved, for a reason this build does not know: it did not run.
    NotRun,
    /// Not answered yet.
    Pending,
    /// Events were lost while it was pending, and its time is up: core answered it somehow.
    Unknown,
    /// This session never got this token (or has forgotten it).
    NotYours,
}

struct Claimed {
    owner: Owner,
    expires_at_ms: i64,
    answer: Option<ConfirmationOutcome>,
    /// Events were lost while it was pending.
    lost: bool,
    claimed_at: Instant,
}

#[derive(Default)]
struct State {
    claimed: HashMap<String, Claimed>,
    unclaimed: HashMap<String, (ConfirmationOutcome, Instant)>,
    unclaimed_order: VecDeque<String>,
}

pub(crate) struct Confirmations {
    state: Mutex<State>,
    /// Bumped on every answer, so waiters look again.
    changed: watch::Sender<u64>,
}

impl Confirmations {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(State::default()),
            changed: watch::channel(0).0,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn bump(&self) {
        self.changed.send_modify(|n| *n = n.wrapping_add(1));
    }

    /// Core answered a confirmation (`ConfirmationResolved`).
    pub(crate) fn answered(&self, answer: ConfirmationOutcome) {
        let mut state = self.lock();
        let token = answer.token.clone();
        match state.claimed.get_mut(&token) {
            Some(claimed) => claimed.answer = Some(answer),
            None => {
                let now = Instant::now();
                state.unclaimed.insert(token.clone(), (answer, now));
                state.unclaimed_order.push_back(token);
                while state.unclaimed_order.len() > UNCLAIMED
                    || state.unclaimed_order.front().is_some_and(|t| {
                        state
                            .unclaimed
                            .get(t)
                            .is_none_or(|(_, at)| now.duration_since(*at) > UNCLAIMED_FOR)
                    })
                {
                    if let Some(old) = state.unclaimed_order.pop_front() {
                        state.unclaimed.remove(&old);
                    }
                }
            }
        }
        drop(state);
        self.bump();
    }

    /// Events were dropped (`Resync`): an answer may have been lost.
    pub(crate) fn lost_events(&self) {
        let mut state = self.lock();
        for claimed in state.claimed.values_mut() {
            if claimed.answer.is_none() {
                claimed.lost = true;
            }
        }
        drop(state);
        self.bump();
    }

    /// Core gave `owner` this token (`needs_confirmation`). A token another session claimed
    /// first stays that session's (an idempotent retry from another session of the same
    /// agent gets the same token).
    pub(crate) fn claim(&self, token: &str, owner: &Owner, expires_at_ms: i64) {
        let mut state = self.lock();
        if state.claimed.contains_key(token) {
            return;
        }
        let answer = state.unclaimed.remove(token).map(|(answer, _)| answer);
        state.unclaimed_order.retain(|t| t != token);
        state.claimed.insert(
            token.to_owned(),
            Claimed {
                owner: owner.clone(),
                expires_at_ms,
                answer,
                lost: false,
                claimed_at: Instant::now(),
            },
        );
        // Bound what one session keeps: drop its oldest answered tokens.
        let mut mine: Vec<(Instant, String)> = state
            .claimed
            .iter()
            .filter(|(_, c)| &c.owner == owner && c.answer.is_some())
            .map(|(t, c)| (c.claimed_at, t.clone()))
            .collect();
        let total = state.claimed.values().filter(|c| &c.owner == owner).count();
        if total > PER_SESSION {
            mine.sort();
            for (_, token) in mine.into_iter().take(total - PER_SESSION) {
                state.claimed.remove(&token);
            }
        }
    }

    /// The session ended: forget its tokens.
    pub(crate) fn forget(&self, owner: &Owner) {
        self.lock().claimed.retain(|_, c| &c.owner != owner);
    }

    /// Where `token` stands for `owner`, now.
    fn status(&self, token: &str, owner: &Owner, now_ms: i64) -> Status {
        let state = self.lock();
        let Some(claimed) = state.claimed.get(token).filter(|c| &c.owner == owner) else {
            return Status::NotYours;
        };
        match &claimed.answer {
            Some(outcome) if outcome.approved => match &outcome.result {
                Some(result) => Status::Approved(result.clone()),
                // Approved without a result does not happen; say what is known.
                None => Status::Unknown,
            },
            // Core says why (`reason`, minor 1).
            Some(outcome) => match outcome.reason {
                Some(ConfirmationReason::Rejected) => Status::Rejected,
                Some(ConfirmationReason::Expired) => Status::Expired,
                Some(ConfirmationReason::Withdrawn) => Status::Withdrawn,
                _ => Status::NotRun,
            },
            None if claimed.lost && now_ms > claimed.expires_at_ms => Status::Unknown,
            None => Status::Pending,
        }
    }

    /// Waits until `token` is answered, at most `timeout` (or until `stop` resolves):
    /// [`Status::Pending`] when time is up first.
    pub(crate) async fn wait(
        &self,
        token: &str,
        owner: &Owner,
        timeout: Duration,
        stop: impl Future<Output = ()>,
    ) -> Status {
        // Subscribed before looking, so no answer falls in between.
        let mut changed = self.changed.subscribe();
        let deadline = tokio::time::Instant::now() + timeout;
        tokio::pin!(stop);
        loop {
            let status = self.status(token, owner, unix_ms());
            if status != Status::Pending {
                return status;
            }
            tokio::select! {
                result = changed.changed() => {
                    if result.is_err() {
                        return Status::Pending;
                    }
                }
                // Looked at once more: a token past its expiry after lost events is
                // `Unknown` now, not at the next call.
                () = tokio::time::sleep_until(deadline) => {
                    return self.status(token, owner, unix_ms());
                }
                () = &mut stop => return Status::Pending,
            }
        }
    }
}

pub(crate) fn unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

#[cfg(test)]
mod tests {
    use mpmp_core::contract::Receipt;

    use super::*;

    fn owner(session: &str) -> Owner {
        Owner {
            agent: "a".into(),
            session: session.into(),
        }
    }

    fn outcome(token: &str, reason: ConfirmationReason) -> ConfirmationOutcome {
        let approved = reason == ConfirmationReason::Approved;
        ConfirmationOutcome {
            token: token.into(),
            approved,
            result: approved.then(|| Box::new(CommandResult::Ok(Receipt::default()))),
            reason: Some(reason),
        }
    }

    #[tokio::test]
    async fn only_the_session_that_got_a_token_learns_its_answer() {
        let c = Confirmations::new();
        let far = unix_ms() + 60_000;
        // Answered before it was claimed.
        c.answered(outcome("t1", ConfirmationReason::Approved));
        c.claim("t1", &owner("s1"), far);
        c.claim("t1", &owner("s2"), far);
        let stop = std::future::pending();
        let wait = Duration::from_millis(10);
        assert!(matches!(
            c.wait("t1", &owner("s1"), wait, stop).await,
            Status::Approved(_)
        ));
        assert_eq!(
            c.wait("t1", &owner("s2"), wait, std::future::pending())
                .await,
            Status::NotYours
        );
        // Rejected, expired, withdrawn, pending: as core says (whenever it says it).
        c.claim("t2", &owner("s1"), far);
        assert_eq!(
            c.wait("t2", &owner("s1"), wait, std::future::pending())
                .await,
            Status::Pending
        );
        c.answered(outcome("t2", ConfirmationReason::Rejected));
        assert_eq!(c.status("t2", &owner("s1"), unix_ms()), Status::Rejected);
        // Rejected just before its expiry is still a rejection.
        c.claim("t3", &owner("s1"), unix_ms() - 10);
        c.answered(outcome("t3", ConfirmationReason::Rejected));
        assert_eq!(c.status("t3", &owner("s1"), unix_ms()), Status::Rejected);
        c.claim("t5", &owner("s1"), far);
        c.answered(outcome("t5", ConfirmationReason::Expired));
        assert_eq!(c.status("t5", &owner("s1"), unix_ms()), Status::Expired);
        c.claim("t6", &owner("s1"), far);
        c.answered(outcome("t6", ConfirmationReason::Withdrawn));
        assert_eq!(c.status("t6", &owner("s1"), unix_ms()), Status::Withdrawn);
        // A core that gives no reason (or one this build does not know): it did not run.
        c.claim("t7", &owner("s1"), far);
        c.answered(ConfirmationOutcome {
            reason: None,
            ..outcome("t7", ConfirmationReason::Rejected)
        });
        assert_eq!(c.status("t7", &owner("s1"), unix_ms()), Status::NotRun);
        // Lost events: unknown once its time is up, also when a wait ends on it.
        c.claim("t4", &owner("s1"), unix_ms() + 100);
        c.lost_events();
        assert_eq!(c.status("t4", &owner("s1"), unix_ms()), Status::Pending);
        assert_eq!(
            c.wait(
                "t4",
                &owner("s1"),
                Duration::from_millis(300),
                std::future::pending()
            )
            .await,
            Status::Unknown
        );
        c.forget(&owner("s1"));
        assert_eq!(c.status("t2", &owner("s1"), unix_ms()), Status::NotYours);
    }

    #[tokio::test]
    async fn a_waiter_wakes_on_the_answer() {
        let c = std::sync::Arc::new(Confirmations::new());
        c.claim("t", &owner("s"), unix_ms() + 60_000);
        let waiter = {
            let c = c.clone();
            tokio::spawn(async move {
                c.wait(
                    "t",
                    &owner("s"),
                    Duration::from_secs(30),
                    std::future::pending(),
                )
                .await
            })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        c.answered(outcome("t", ConfirmationReason::Rejected));
        let status = tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(status, Status::Rejected);
    }
}
