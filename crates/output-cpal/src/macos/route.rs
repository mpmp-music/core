//! CoreAudio facts only; no playback policy and no native calls.
use mpmp_engine::output::{DeviceId, OutputEvent, OutputEventSink, RouteFacts};
use std::time::Instant;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Properties {
    pub alive: Option<bool>,
    pub jack: Option<bool>,
    pub source: Option<u32>,
}

impl Properties {
    pub fn available_since(self, old: Self) -> Option<bool> {
        if self.alive == Some(false)
            || (old.alive == Some(true) && self.alive != Some(true))
            || (old.jack == Some(true) && self.jack == Some(false))
        {
            Some(false)
        } else if self.alive != Some(true)
            || old.source != self.source
            || (old.jack == Some(true) && self.jack.is_none())
        {
            None
        } else {
            Some(true)
        }
    }
}

/// Move the entire watched device, preserving both listeners and the comparison baseline.
pub(super) fn take_watched<T>(
    current: &mut Option<T>,
    previous: &mut Vec<(T, Instant)>,
    object: u32,
    object_of: impl Fn(&T) -> u32,
) -> Option<T> {
    if current.as_ref().is_some_and(|d| object_of(d) == object) {
        current.take()
    } else {
        previous
            .iter()
            .position(|(d, _)| object_of(d) == object)
            .map(|index| previous.remove(index).0)
    }
}

pub(super) fn find_uid(
    selected: &str,
    devices: impl IntoIterator<Item = (u32, Option<String>)>,
) -> Option<u32> {
    devices
        .into_iter()
        .find(|(_, id)| id.as_deref() == Some(selected))
        .map(|(object, _)| object)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FixedChange {
    Missing,
    Reconnected(u32),
}

pub(super) fn fixed_change(
    follows_default: bool,
    current: Option<u32>,
    found: Option<u32>,
) -> Option<FixedChange> {
    if follows_default || current == found {
        return None;
    }
    Some(match found {
        Some(object) => FixedChange::Reconnected(object),
        None => FixedChange::Missing,
    })
}

/// A route identity wakes exhausted preparation without claiming a default switch.
/// Unknown makes replacement conservative: PauseClose, then silent preparation.
pub(super) fn fixed_facts(change: FixedChange, old: Properties) -> RouteFacts {
    match change {
        FixedChange::Missing => transition(old, Properties::default(), false),
        FixedChange::Reconnected(_) => RouteFacts::default(),
    }
}

pub(super) fn transition(old: Properties, new: Properties, source_notice: bool) -> RouteFacts {
    let available = new.available_since(old);
    RouteFacts {
        // On macOS this means the current output disconnected, including speakers.
        // The latch is consumed by core; render does not read it or mute immediately.
        headphones_disconnected: (old.alive == Some(true) && new.alive != Some(true))
            || (old.jack == Some(true) && new.jack == Some(false)),
        old_route_available: if source_notice && available != Some(false) {
            None
        } else {
            available
        },
        ..Default::default()
    }
}

pub(super) fn default_transition(available: Option<bool>) -> RouteFacts {
    RouteFacts {
        default_changed: true,
        old_route_available: available,
        // HAL may deliver dOut before livn, including for a recently replaced route.
        headphones_disconnected: available == Some(false),
        // DefaultOutput AudioUnit migrates itself; core still checks the format.
        rebuild_required: false,
        ..Default::default()
    }
}

pub(super) fn publish(
    sink: &OutputEventSink,
    id: Option<String>,
    facts: RouteFacts,
    unknown: bool,
) {
    let facts = RouteFacts {
        route: id.map(DeviceId),
        ..facts
    };
    tracing::debug!(?facts, unknown, "macOS route facts");
    sink.report_route(facts);
    if unknown {
        sink.report(OutputEvent::Unknown);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mpmp_engine::output::policy;

    #[test]
    fn source_and_same_object_notices_stay_conservative_even_with_equal_properties() {
        let old = Properties {
            alive: Some(true),
            jack: None,
            source: Some(10),
        };
        // Initial sampling may already contain the post-disconnect source. Equality
        // does not establish that the physical route change was handled.
        for source in [Some(10), Some(20), None] {
            let new = Properties { source, ..old };
            let facts = transition(old, new, true);
            assert_eq!(facts.old_route_available, None);
            assert!(!facts.headphones_disconnected);
            let (tx, _) = crossbeam_channel::bounded(1);
            let sink = OutputEventSink::new(1, tx);
            publish(&sink, Some("coreaudio:built-in".into()), facts, true);
            let facts = sink.take().unwrap();
            assert!(facts.contains(OutputEvent::Unknown));
            let mut policy = policy::OutputPolicy::default();
            assert_eq!(
                policy.facts(facts, 0, 0, 0),
                Some(policy::Action::PauseClose)
            );
        }
    }

    #[test]
    fn jack_loss_wins_even_if_device_remains_alive() {
        let old = Properties {
            alive: Some(true),
            jack: Some(true),
            source: Some(10),
        };
        let new = Properties {
            jack: Some(false),
            source: Some(20),
            ..old
        };
        let facts = transition(old, new, false);
        assert!(facts.headphones_disconnected);
        assert_eq!(facts.old_route_available, Some(false));
        assert_eq!(old.available_since(old), Some(true));
        assert_eq!(
            Properties {
                alive: Some(false),
                ..old
            }
            .available_since(old),
            Some(false)
        );
        assert_eq!(Properties::default().available_since(old), Some(false));
        assert_eq!(
            transition(
                old,
                Properties {
                    alive: Some(false),
                    ..old
                },
                true
            )
            .old_route_available,
            Some(false)
        );
    }

    #[test]
    fn vanished_device_queries_are_a_disconnect_even_without_jack_support() {
        let old = Properties {
            alive: Some(true),
            ..Default::default()
        };
        for alive in [None, Some(false)] {
            let new = Properties {
                alive,
                ..Default::default()
            };
            for source_notice in [false, true] {
                let facts = transition(old, new, source_notice);
                assert!(facts.headphones_disconnected);
                assert_eq!(facts.old_route_available, Some(false));
            }
        }
        let unknown = Properties::default();
        assert_eq!(unknown.available_since(unknown), None);
        assert!(!transition(unknown, unknown, false).headphones_disconnected);
        assert!(!transition(old, old, false).headphones_disconnected);
    }

    #[test]
    fn loss_and_default_callbacks_merge_in_either_order() {
        for loss_first in [true, false] {
            let (tx, _) = crossbeam_channel::bounded(1);
            let sink = OutputEventSink::new(1, tx);
            let loss = transition(
                Properties {
                    alive: Some(true),
                    ..Default::default()
                },
                Properties::default(),
                false,
            );
            let change = default_transition(Some(true));
            let facts = if loss_first {
                [loss, change]
            } else {
                [change, loss]
            };
            for fact in facts {
                publish(&sink, Some("coreaudio:device".into()), fact, false);
            }
            let merged = sink.take().unwrap();
            assert!(merged.route.default_changed);
            assert!(merged.route.headphones_disconnected);
            assert_eq!(merged.route.old_route_available, Some(false));
            assert!(!merged.route.rebuild_required);
            assert!(sink.take_disconnect());
        }
    }

    #[test]
    fn default_notification_alone_latches_a_known_loss() {
        for available in [None, Some(true), Some(false)] {
            let (tx, _) = crossbeam_channel::bounded(1);
            let sink = OutputEventSink::new(1, tx);
            publish(
                &sink,
                Some("coreaudio:new-route".into()),
                default_transition(available),
                false,
            );
            assert_eq!(sink.take_disconnect(), available == Some(false));
            let facts = sink.take().unwrap().route;
            assert!(facts.default_changed);
            assert_eq!(facts.old_route_available, available);
            assert_eq!(facts.headphones_disconnected, available == Some(false));
        }
    }
}

// Preserve the policy tests formerly compiled through cross-crate inclusion.
#[cfg(test)]
mod policy;

#[cfg(test)]
mod regressions {
    use super::*;
    use mpmp_engine::output::policy::{Action, OutputPolicy};

    #[derive(Debug, PartialEq, Eq)]
    struct Watched {
        object: u32,
        properties: Properties,
    }

    #[test]
    fn reused_watch_preserves_queued_jack_and_alive_disconnects() {
        let baseline = Properties {
            alive: Some(true),
            jack: Some(true),
            source: None,
        };
        for from_previous in [false, true] {
            for observed in [
                Properties {
                    jack: Some(false),
                    ..baseline
                },
                Properties {
                    alive: Some(false),
                    ..baseline
                },
                Properties {
                    alive: None,
                    jack: None,
                    source: None,
                },
            ] {
                let watched = Watched {
                    object: 98,
                    properties: baseline,
                };
                let (mut current, mut previous) = if from_previous {
                    (None, vec![(watched, Instant::now())])
                } else {
                    (Some(watched), Vec::new())
                };
                // The loss is already queued after the initial drain. Reconfiguration
                // must move this watch without querying away its comparison baseline.
                current = take_watched(&mut current, &mut previous, 98, |d| d.object);
                let device = current.as_mut().unwrap();
                assert_eq!(device.properties, baseline);
                let facts = transition(device.properties, observed, false);
                device.properties = observed;
                let (tx, _) = crossbeam_channel::bounded(1);
                let sink = OutputEventSink::new(1, tx);
                publish(&sink, Some("coreaudio:fixed".into()), facts, true);
                assert!(sink.take_disconnect());
                let facts = sink.take().unwrap();
                assert!(facts.route.headphones_disconnected);
                assert_eq!(facts.route.old_route_available, Some(false));
                assert!(previous.is_empty());
            }
        }
    }

    #[test]
    fn fixed_uid_reappears_on_a_new_non_default_object() {
        let selected = "coreaudio:usb-uid";
        // The system default is a different UID. It must never satisfy this lookup.
        let objects = || {
            [
                (42, Some("coreaudio:builtin".into())),
                (139, Some(selected.into())),
            ]
        };
        assert_eq!(
            find_uid(selected, [(42, Some("coreaudio:builtin".into()))]),
            None
        );
        let found = find_uid(selected, objects());
        assert_eq!(found, Some(139));
        assert_eq!(
            fixed_change(false, Some(98), found),
            Some(FixedChange::Reconnected(139))
        );
        assert_eq!(
            fixed_change(false, None, found),
            Some(FixedChange::Reconnected(139))
        );
        assert_eq!(fixed_change(false, Some(139), found), None);
        assert_eq!(
            fixed_change(false, Some(139), None),
            Some(FixedChange::Missing)
        );
        assert_eq!(fixed_change(false, None, None), None);
        assert_eq!(fixed_change(true, Some(98), found), None);
        assert_eq!(fixed_change(true, Some(98), None), None);
    }

    #[test]
    fn fixed_reconnection_reopens_an_exhausted_preparation_budget_conservatively() {
        let mut policy = OutputPolicy::default();
        policy.prepare_output();
        assert!(policy.begin_attempt(0, Action::Rebuild));
        for at in [300, 1300, 4300] {
            assert_eq!(policy.poll(at, 1, 0, 0, true), Some(Action::Rebuild));
            assert!(policy.begin_attempt(at, Action::Rebuild));
            policy.unavailable(at);
        }
        for attempt in 1..=20 {
            let at = 4300 + attempt * 15_000;
            assert_eq!(policy.poll(at, 1, 0, 0, true), Some(Action::PauseRebuild));
            assert!(policy.begin_attempt(at, Action::Rebuild));
            policy.unavailable(at);
        }
        assert_eq!(policy.next_due(), None);
        assert!(!policy.begin_attempt(400_000, Action::Rebuild));

        let id = "coreaudio:usb-uid";
        let found = find_uid(
            id,
            [
                (42, Some("coreaudio:builtin".into())),
                (195, Some(id.into())),
            ],
        );
        let change = fixed_change(false, None, found).unwrap();
        let (tx, _) = crossbeam_channel::bounded(1);
        let sink = OutputEventSink::new(1, tx);
        publish(
            &sink,
            Some(id.into()),
            fixed_facts(change, Properties::default()),
            true,
        );
        let facts = sink.take().unwrap();
        assert_eq!(facts.route.route, Some(DeviceId(id.into())));
        assert!(!facts.route.default_changed);
        assert!(!facts.route.headphones_disconnected);
        assert!(facts.contains(OutputEvent::Unknown));
        // This is on_output's real ordering: route identity resets the budget,
        // Unknown selects PauseClose, and a retained output_at allows Rebuild.
        policy.external_change();
        assert_eq!(policy.facts(facts, 400_001, 0, 0), Some(Action::PauseClose));
        assert!(policy.begin_attempt(400_001, Action::Rebuild));
    }
}

/// Inventory defaults change independently of the selected stream's routing policy.
pub(super) fn default_inventory_changed(sink: &mpmp_engine::output::OutputEventSink) {
    sink.report_route(mpmp_engine::output::RouteFacts {
        devices_changed: true,
        ..Default::default()
    });
}

#[cfg(test)]
mod inventory_tests {
    #[test]
    fn fixed_stream_default_change_invalidates_inventory_without_following_route() {
        let (tx, _) = crossbeam_channel::bounded(1);
        let sink = mpmp_engine::output::OutputEventSink::new(1, tx);
        super::default_inventory_changed(&sink);
        let facts = sink.take().unwrap();
        assert!(facts.route.devices_changed);
        assert!(!facts.route.default_changed);
        assert!(facts.route.route.is_none());
        assert!(!facts.route.rebuild_required);
    }
}
