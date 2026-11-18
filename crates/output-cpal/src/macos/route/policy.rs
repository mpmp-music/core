//! Output policy decisions the macOS route monitor relies on, checked against the engine.
use mpmp_engine::output::policy::*;
use mpmp_engine::output::{OutputEvent, OutputFacts};

#[cfg(test)]
mod tests {
    use super::*;
    use mpmp_engine::output::{OutputEventSink, RouteFacts};

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
