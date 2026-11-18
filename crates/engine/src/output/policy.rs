//! Platform-independent output decisions. Time is supplied by the actor/tests.
//!
//! 内部共享，供 mpmp-core 与 mpmp-output-cpal 使用，不属于稳定 API。
use crate::output::{OutputEvent, OutputFacts};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    PauseClose,
    PauseRebuild,
    PauseCheckFormat,
    Follow,
    CheckFormat,
    #[default]
    Rebuild,
}

#[derive(Default)]
pub struct OutputPolicy {
    observation: Option<Observation>,
    retry: Option<(u64, usize)>,
    retry_ready: bool,
    retry_action: Action,
    exhausted: bool,
    slow_at: Option<u64>,
    slow_left: u8,
}

struct Observation {
    generation: u64,
    due: u64,
    callbacks: u64,
    played: u64,
    explicit_switch: bool,
    rebuild_required: bool,
    tail_sampled: bool,
}

impl OutputPolicy {
    pub fn next_due(&self) -> Option<u64> {
        self.observation
            .as_ref()
            .map(|o| if o.tail_sampled { o.due } else { o.due - 100 })
            .into_iter()
            .chain(self.retry.map(|(at, _)| at))
            .chain(self.slow_at)
            .min()
    }

    pub fn facts(
        &mut self,
        facts: OutputFacts,
        now: u64,
        callbacks: u64,
        played: u64,
    ) -> Option<Action> {
        if facts.route.headphones_disconnected || facts.contains(OutputEvent::DeviceUnplugged) {
            self.cancel();
            return Some(Action::PauseClose);
        }
        if facts.route.default_changed && facts.route.old_route_available == Some(true) {
            // Coalesce native default and endpoint-loss notifications before reopening.
            if let Some(o) = &mut self.observation {
                o.explicit_switch = true;
                o.rebuild_required |= facts.route.rebuild_required;
            } else {
                self.observation = Some(Observation {
                    generation: facts.generation,
                    due: now + 300,
                    callbacks,
                    played,
                    explicit_switch: true,
                    rebuild_required: facts.route.rebuild_required,
                    tail_sampled: false,
                });
            }
            return None;
        }
        let format_signal =
            facts.route.format_changed || facts.contains(OutputEvent::ClockUnreliable);
        if format_signal
            && !facts.route.default_changed
            && !facts.contains(OutputEvent::DeviceUnavailable)
            && !facts.contains(OutputEvent::Rerouted)
            && !facts.contains(OutputEvent::Unknown)
        {
            // Known format/clock trouble is not an unknown route loss. An explicit
            // switch still finishes its disconnect-coalescing observation first.
            if self.observation.as_ref().is_some_and(|o| o.explicit_switch) {
                return None;
            }
            self.observation = None;
            return Some(Action::CheckFormat);
        }
        if facts.contains(OutputEvent::StreamInvalidated) {
            if self.observation.is_none() {
                self.observation = Some(Observation {
                    generation: facts.generation,
                    due: now + 300,
                    callbacks,
                    played,
                    explicit_switch: false,
                    rebuild_required: facts.route.rebuild_required,
                    tail_sampled: false,
                });
            }
            return None;
        }
        if facts.contains(OutputEvent::DeviceUnavailable) {
            self.observation = None;
            return Some(Action::PauseRebuild);
        }
        if facts.contains(OutputEvent::Rerouted) {
            if self.observation.as_ref().is_some_and(|o| o.explicit_switch) {
                return None;
            }
            return Some(Action::PauseCheckFormat);
        }
        if facts.route.default_changed || facts.contains(OutputEvent::Unknown) {
            self.observation = None;
            return Some(Action::PauseClose);
        }
        None // Xrun is diagnostic only.
    }

    pub fn poll(
        &mut self,
        now: u64,
        generation: u64,
        callbacks: u64,
        played: u64,
        paused: bool,
    ) -> Option<Action> {
        if let Some(o) = &mut self.observation
            && !o.tail_sampled
            && now >= o.due - 100
            && now < o.due
        {
            o.callbacks = callbacks;
            o.played = played;
            o.tail_sampled = true;
        }
        if self.observation.as_ref().is_some_and(|o| now >= o.due) {
            let o = self.observation.take().unwrap();
            if o.generation != generation {
                return None;
            }
            let alive = o.tail_sampled && callbacks > o.callbacks && (paused || played > o.played);
            return Some(match (o.explicit_switch, alive) {
                (true, _) if o.rebuild_required => Action::Follow,
                (true, true) => Action::CheckFormat,
                (true, false) => Action::Follow,
                (false, true) => Action::PauseCheckFormat,
                (false, false) => Action::PauseRebuild,
            });
        }
        if let Some((due, attempt)) = self.retry
            && now >= due
        {
            self.retry_ready = true;
            self.exhausted = attempt == 2;
            self.retry = if attempt < 2 {
                Some((now + [300, 1000, 3000][attempt + 1], attempt + 1))
            } else {
                None
            };
            return Some(self.retry_action);
        }
        if self.slow_at.is_some_and(|at| now >= at) {
            self.slow_at = None;
            self.slow_left -= 1;
            self.retry_ready = true;
            return Some(Action::PauseRebuild);
        }
        None
    }

    /// A failed output starts finite, silent background preparation.
    pub fn prepare_output(&mut self) {
        self.slow_left = 20;
    }

    /// Returns whether a failed attempt has exhausted the fast phase. The actor
    /// must pause immediately, before any slow timer or external notification.
    pub fn unavailable(&mut self, now: u64) -> bool {
        if self.retry.is_none() && !self.exhausted {
            self.retry = Some((now + 300, 0));
        } else if self.exhausted
            && self.retry.is_none()
            && self.slow_left > 0
            && self.slow_at.is_none()
        {
            self.slow_at = Some(now + 15_000);
        }
        self.exhausted && self.retry.is_none()
    }

    /// Format queries and rebuilds share one deadline and finite attempt budget.
    pub fn begin_attempt(&mut self, now: u64, action: Action) -> bool {
        if action == Action::Rebuild {
            self.rebuild_required();
        } else if self.retry.is_none() && !self.exhausted {
            self.retry_action = action;
        }
        if std::mem::take(&mut self.retry_ready) {
            return true;
        }
        if self.retry.is_some() || self.exhausted {
            return false;
        }
        self.unavailable(now);
        true
    }

    pub fn rebuild_required(&mut self) {
        self.retry_action = Action::Rebuild;
    }

    pub fn external_change(&mut self) {
        self.slow_at = None;
        self.retry = None;
        self.retry_ready = false;
        self.retry_action = Action::Rebuild;
        self.exhausted = false;
    }

    /// A real property change can reopen an exhausted budget, but cannot shorten
    /// an in-flight retry deadline. Callback clock faults never renew the budget.
    pub fn format_change(&mut self) {
        if self.exhausted && self.retry.is_none() {
            self.external_change();
        }
    }

    pub fn recovered(&mut self) {
        self.slow_left = 0;
        self.external_change();
    }
    pub fn cancel(&mut self) {
        self.slow_left = 0;
        self.external_change();
        self.observation = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::{OutputEventSink, RouteFacts};

    fn facts(event: Option<OutputEvent>, route: RouteFacts) -> OutputFacts {
        let (tx, _rx) = crossbeam_channel::bounded(1);
        let sink = OutputEventSink::new(1, tx);
        if let Some(event) = event {
            sink.report(event);
        }
        sink.report_route(route);
        sink.take().unwrap()
    }

    #[test]
    fn observation_counts_callbacks_and_content_without_sleeping() {
        for (callbacks, played, paused, expected) in [
            (11, 21, false, Action::PauseCheckFormat),
            (10, 20, false, Action::PauseRebuild),
            (11, 20, true, Action::PauseCheckFormat),
            (11, 20, false, Action::PauseRebuild),
        ] {
            let mut policy = OutputPolicy::default();
            assert_eq!(
                policy.facts(
                    facts(Some(OutputEvent::StreamInvalidated), RouteFacts::default()),
                    0,
                    10,
                    20
                ),
                None
            );
            assert_eq!(policy.poll(200, 1, 10, 20, paused), None);
            assert_eq!(policy.poll(250, 1, callbacks, played, paused), None);
            assert_eq!(
                policy.poll(300, 1, callbacks, played, paused),
                Some(expected)
            );
        }
    }

    #[test]
    fn format_signals_keep_transport_but_never_override_route_safety() {
        for (event, expected) in [
            (OutputEvent::ClockUnreliable, Action::CheckFormat),
            (OutputEvent::StreamInvalidated, Action::CheckFormat),
            (OutputEvent::Unknown, Action::PauseClose),
            (OutputEvent::DeviceUnplugged, Action::PauseClose),
            (OutputEvent::DeviceUnavailable, Action::PauseRebuild),
            (OutputEvent::Rerouted, Action::PauseCheckFormat),
        ] {
            let mut policy = OutputPolicy::default();
            assert_eq!(
                policy.facts(
                    facts(
                        Some(event),
                        RouteFacts {
                            format_changed: true,
                            ..Default::default()
                        }
                    ),
                    0,
                    0,
                    0
                ),
                Some(expected)
            );
        }
        let mut policy = OutputPolicy::default();
        policy.facts(
            facts(Some(OutputEvent::StreamInvalidated), RouteFacts::default()),
            0,
            0,
            0,
        );
        assert_eq!(
            policy.facts(
                facts(
                    None,
                    RouteFacts {
                        format_changed: true,
                        ..Default::default()
                    }
                ),
                1,
                0,
                0
            ),
            Some(Action::CheckFormat)
        );
        assert!(policy.next_due().is_none());
        policy.facts(
            facts(
                None,
                RouteFacts {
                    default_changed: true,
                    old_route_available: Some(true),
                    ..Default::default()
                },
            ),
            2,
            0,
            0,
        );
        let due = policy.next_due();
        assert_eq!(
            policy.facts(
                facts(Some(OutputEvent::ClockUnreliable), RouteFacts::default()),
                3,
                0,
                0
            ),
            None
        );
        assert_eq!(policy.next_due(), due);
    }

    #[test]
    fn unplug_wins_in_either_notification_order_and_cancels_observation() {
        for unplug_first in [false, true] {
            let (tx, _rx) = crossbeam_channel::bounded(1);
            let sink = OutputEventSink::new(1, tx);
            let loss = RouteFacts {
                headphones_disconnected: true,
                old_route_available: Some(false),
                ..Default::default()
            };
            let switch = RouteFacts {
                default_changed: true,
                old_route_available: Some(true),
                ..Default::default()
            };
            for f in if unplug_first {
                [loss, switch]
            } else {
                [switch, loss]
            } {
                sink.report_route(f);
            }
            let mut policy = OutputPolicy::default();
            assert_eq!(
                policy.facts(sink.take().unwrap(), 0, 0, 0),
                Some(Action::PauseClose)
            );
            assert_eq!(policy.poll(500, 1, 1, 1, false), None);
        }
    }

    #[test]
    fn explicit_switch_preserves_transport_and_does_not_extend_deadline() {
        let mut policy = OutputPolicy::default();
        policy.facts(
            facts(Some(OutputEvent::StreamInvalidated), RouteFacts::default()),
            0,
            1,
            1,
        );
        let switch = RouteFacts {
            default_changed: true,
            old_route_available: Some(true),
            ..Default::default()
        };
        policy.facts(facts(None, switch.clone()), 100, 1, 1);
        assert_eq!(policy.poll(200, 1, 1, 1, false), None);
        policy.facts(facts(None, switch), 299, 1, 1);
        assert_eq!(policy.poll(300, 1, 2, 2, false), Some(Action::CheckFormat));
        let mut policy = OutputPolicy::default();
        policy.facts(
            facts(Some(OutputEvent::StreamInvalidated), RouteFacts::default()),
            0,
            1,
            1,
        );
        assert_eq!(policy.poll(300, 2, 2, 2, false), None);
    }

    #[test]
    fn early_progress_does_not_hide_a_dead_tail() {
        for paused in [false, true] {
            let mut policy = OutputPolicy::default();
            policy.facts(
                facts(Some(OutputEvent::StreamInvalidated), RouteFacts::default()),
                0,
                1,
                1,
            );
            assert_eq!(policy.poll(10, 1, 2, 2, paused), None);
            assert_eq!(policy.poll(200, 1, 2, 2, paused), None);
            assert_eq!(
                policy.poll(300, 1, 2, 2, paused),
                Some(Action::PauseRebuild)
            );
        }
    }

    #[test]
    fn retries_are_finite_and_xrun_is_diagnostic() {
        let mut policy = OutputPolicy::default();
        assert_eq!(
            policy.facts(
                facts(Some(OutputEvent::Xrun), RouteFacts::default()),
                0,
                0,
                0
            ),
            None
        );
        policy.unavailable(0);
        for (before, due) in [(299, 300), (1299, 1300), (4299, 4300)] {
            assert_eq!(policy.poll(before, 1, 0, 0, true), None);
            assert_eq!(policy.poll(due, 1, 0, 0, true), Some(Action::Rebuild));
        }
        assert_eq!(policy.poll(100_000, 1, 0, 0, true), None);
    }
}

#[cfg(test)]
mod recovery_tests {
    use super::*;

    #[test]
    fn gentle_retry_has_exact_intervals_finite_budget_and_no_event_shortcut() {
        let mut policy = OutputPolicy::default();
        policy.prepare_output();
        assert!(policy.begin_attempt(0, Action::Rebuild));
        for at in [300, 1300, 4300] {
            assert_eq!(policy.poll(at, 1, 0, 0, false), Some(Action::Rebuild));
            assert!(policy.begin_attempt(at, Action::Rebuild));
            policy.unavailable(at);
        }
        for attempt in 1..=20 {
            let at = 4300 + attempt * 15_000;
            assert_eq!(policy.next_due(), Some(at));
            assert_eq!(policy.poll(at - 1, 1, 0, 0, false), None);
            assert!(!policy.begin_attempt(at - 1, Action::CheckFormat));
            assert_eq!(policy.poll(at, 1, 0, 0, false), Some(Action::PauseRebuild));
            assert!(policy.begin_attempt(at, Action::Rebuild));
            policy.unavailable(at);
        }
        assert_eq!(policy.next_due(), None);
        policy.prepare_output();
        policy.unavailable(400_000);
        assert_eq!(policy.next_due(), Some(415_000));
        policy.cancel();
        assert_eq!(policy.next_due(), None);
        policy.external_change();
        assert!(policy.begin_attempt(400_001, Action::Rebuild));
    }
}
