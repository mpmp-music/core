use mpmp_engine::output::{DeviceId, OutputEventSink, RouteFacts};
use std::time::{Duration, Instant};

#[derive(Default)]
pub(super) struct Route {
    pub sink: Option<OutputEventSink>,
    pub identity: super::identity::ActiveIdentity,
    pub endpoint: String,
    pub headphones: bool,
    pub follows_default: bool,
    /// Follows the user's default preference, but the current stream is endpoint-bound.
    pub pinned_default: bool,
    pub previous: Option<(String, bool, Instant)>,
}

impl Route {
    /// An explicit new selection supersedes loss association with the earlier
    /// endpoint. Automatic default reconstruction retains that safety association.
    pub fn selection_changed(&mut self) {
        self.previous = None;
    }

    /// Device-added notifications have no playback authority. Wake preparation only
    /// when the requested fixed identity is actually available again.
    pub fn fixed_available(&self, available: Option<bool>) {
        if !self.follows_default
            && available == Some(true)
            && let Some(sink) = &self.sink
        {
            sink.report_route(RouteFacts {
                route: Some(DeviceId(self.endpoint.clone())),
                ..Default::default()
            });
        }
    }

    pub fn format_changed(&self, id: &str) {
        // Property notifications for an earlier or unrelated endpoint are irrelevant.
        if !id.is_empty()
            && id == self.endpoint
            && let Some(sink) = &self.sink
        {
            sink.report_route(RouteFacts {
                format_changed: true,
                ..Default::default()
            });
        }
    }

    pub fn lost(
        &self,
        id: &str,
        removed: bool,
        unavailable: bool,
        disconnected: bool,
        at: Instant,
    ) {
        let headphones = if self.endpoint == id {
            Some(self.headphones)
        } else {
            self.previous
                .as_ref()
                .filter(|(old, _, changed)| {
                    old == id
                        && at.saturating_duration_since(*changed) <= Duration::from_millis(300)
                })
                .map(|(_, h, _)| *h)
        };
        if let (Some(headphones), Some(sink)) = (headphones, &self.sink) {
            sink.report_route(RouteFacts {
                route: Some(DeviceId(self.endpoint.clone())),
                headphones_disconnected: headphones && (removed || disconnected),
                old_route_available: Some(!unavailable),
                ..RouteFacts::default()
            });
            if unavailable {
                sink.report(mpmp_engine::output::OutputEvent::DeviceUnavailable);
            }
        }
    }
}

impl Route {
    pub fn configure(
        &mut self,
        endpoint: String,
        headphones: bool,
        follows_default: bool,
        automatic: bool,
        at: Instant,
    ) {
        self.replace(endpoint, headphones, at);
        self.follows_default = follows_default;
        self.pinned_default = follows_default && !automatic;
    }

    fn replace(&mut self, endpoint: String, headphones: bool, at: Instant) {
        if self.endpoint != endpoint {
            self.previous = Some((self.endpoint.clone(), self.headphones, at));
            self.endpoint = endpoint;
        }
        self.headphones = headphones;
    }

    pub fn default_changed(
        &mut self,
        id: Option<String>,
        headphones: bool,
        available: Option<bool>,
        at: Instant,
    ) {
        if !self.follows_default {
            return;
        }
        if let Some(sink) = &self.sink {
            sink.report_route(RouteFacts {
                route: id.clone().map(DeviceId),
                default_changed: id.as_ref().is_none_or(|id| *id != self.endpoint),
                rebuild_required: self.pinned_default
                    && id.as_ref().is_none_or(|id| *id != self.endpoint),
                old_route_available: available,
                ..Default::default()
            });
        }
        if !self.pinned_default {
            self.replace(id.unwrap_or_default(), headphones, at);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_default_keeps_actual_endpoint_until_rebuilt_and_latches_its_disconnect() {
        let (tx, _) = crossbeam_channel::bounded(1);
        let sink = OutputEventSink::new(0, tx);
        let mut route = Route {
            sink: Some(sink.clone()),
            ..Default::default()
        };
        let at = Instant::now();
        route.configure("A".into(), true, true, false, at);
        let identity = mpmp_engine::output::OutputIdentity::new(mpmp_engine::output::DeviceInfo {
            id: DeviceId("A".into()),
            name: "A".into(),
            is_default: true,
            persistent: true,
        });
        route.identity.attach(Some(identity.clone()), false);
        route.identity.confirm(
            Some(mpmp_engine::output::DeviceInfo {
                id: DeviceId("B".into()),
                name: "B".into(),
                is_default: true,
                persistent: true,
            }),
            &sink,
        );
        assert_eq!(identity.get().unwrap().id.0, "A");
        route.default_changed(Some("B".into()), false, Some(true), at);
        let facts = sink.take().unwrap();
        assert!(facts.route.default_changed);
        assert!(facts.route.rebuild_required);
        assert_eq!(facts.route.route, Some(DeviceId("B".into())));
        assert_eq!(route.endpoint, "A");
        route.format_changed("B");
        assert!(sink.take().is_none(), "B is not the active stream");
        route.lost("B", true, true, true, at + Duration::from_secs(1));
        assert!(sink.take().is_none());
        route.lost("A", true, true, true, at + Duration::from_secs(1));
        let loss = sink.take().unwrap();
        assert!(loss.route.headphones_disconnected);
        assert_eq!(loss.route.route, Some(DeviceId("A".into())));
        assert!(sink.take_disconnect());
        // A rebuilt virtual default handle can migrate again; the preference is
        // never made sticky by an earlier fallback.
        route.configure("B".into(), false, true, true, at + Duration::from_secs(2));
        route.default_changed(
            Some("C".into()),
            false,
            Some(true),
            at + Duration::from_secs(3),
        );
        assert!(!sink.take().unwrap().route.rebuild_required);
        assert_eq!(route.endpoint, "C");
    }

    #[test]
    fn explicitly_fixed_endpoint_does_not_follow_default_notifications() {
        let (tx, _) = crossbeam_channel::bounded(1);
        let sink = OutputEventSink::new(0, tx);
        let mut route = Route {
            sink: Some(sink.clone()),
            ..Default::default()
        };
        let at = Instant::now();
        route.configure("A".into(), true, false, false, at);
        route.default_changed(Some("B".into()), false, Some(true), at);
        assert!(sink.take().is_none());
        assert_eq!(route.endpoint, "A");
        route.lost("A", true, true, true, at);
        assert!(sink.take().unwrap().route.headphones_disconnected);
    }

    #[test]
    fn format_properties_only_affect_current_endpoint_without_claiming_route_loss() {
        for follows_default in [false, true] {
            let (tx, rx) = crossbeam_channel::bounded(1);
            let sink = OutputEventSink::new(0, tx);
            let mut route = Route {
                sink: Some(sink.clone()),
                endpoint: "A".into(),
                follows_default,
                ..Default::default()
            };
            route.configure(
                "B".into(),
                true,
                follows_default,
                follows_default,
                Instant::now(),
            );
            for id in ["", "A", "unrelated"] {
                route.format_changed(id);
                assert!(sink.take().is_none());
            }
            route.format_changed("B");
            assert_eq!(rx.len(), 1);
            let facts = sink.take().unwrap();
            assert!(facts.route.format_changed);
            assert!(!facts.route.default_changed);
            assert!(!facts.route.headphones_disconnected);
            assert!(facts.route.route.is_none());
        }
    }

    #[test]
    fn configuration_does_not_publish_device_changes() {
        let (tx, _rx) = crossbeam_channel::bounded(1);
        let sink = OutputEventSink::new(0, tx);
        let mut route = Route {
            sink: Some(sink.clone()),
            ..Default::default()
        };
        for _ in 0..10 {
            route.configure("A".into(), true, true, true, Instant::now());
            assert!(sink.take().is_none());
        }
    }

    #[test]
    fn missing_default_publishes_loss_without_an_endpoint_id() {
        let (tx, _rx) = crossbeam_channel::bounded(1);
        let sink = OutputEventSink::new(0, tx);
        let mut route = Route {
            sink: Some(sink.clone()),
            endpoint: "A".into(),
            follows_default: true,
            ..Default::default()
        };
        route.default_changed(None, false, Some(false), Instant::now());
        let facts = sink.take().unwrap();
        assert!(facts.route.default_changed);
        assert_eq!(facts.route.route, None);
        assert_eq!(facts.route.old_route_available, Some(false));
        assert!(route.endpoint.is_empty());
    }

    #[test]
    fn intermediate_default_headphones_loss_survives_a_b_loss_c_batch() {
        let (tx, _rx) = crossbeam_channel::bounded(1);
        let sink = OutputEventSink::new(0, tx);
        let mut route = Route {
            sink: Some(sink.clone()),
            endpoint: "A".into(),
            follows_default: true,
            ..Default::default()
        };
        let at = Instant::now();
        route.default_changed(Some("B".into()), true, Some(true), at);
        route.lost("B", false, true, true, at + Duration::from_millis(1));
        route.default_changed(
            Some("C".into()),
            false,
            Some(false),
            at + Duration::from_millis(2),
        );
        let facts = sink.take().expect("default transitions must publish facts");
        assert!(facts.route.headphones_disconnected);
        assert_eq!(facts.route.route, Some(DeviceId("C".into())));
        assert_eq!(facts.route.old_route_available, Some(false));
    }
}

#[cfg(test)]
mod fixed_arrival_tests {
    use super::*;
    #[test]
    fn explicit_selection_retires_old_endpoint_loss_association() {
        let (tx, _) = crossbeam_channel::bounded(1);
        let sink = OutputEventSink::new(0, tx);
        let mut route = Route {
            sink: Some(sink.clone()),
            ..Default::default()
        };
        let at = Instant::now();
        route.configure("old".into(), true, false, false, at);
        route.configure("new".into(), false, false, false, at);
        route.selection_changed();
        route.lost("old", true, true, true, at);
        assert!(sink.take().is_none());
        assert!(!sink.take_disconnect());
        route.lost("new", true, true, true, at);
        assert!(sink.take().is_some());
    }

    #[test]
    fn only_the_available_fixed_identity_wakes_silent_preparation() {
        let (tx, _) = crossbeam_channel::bounded(1);
        let sink = OutputEventSink::new(0, tx);
        let mut route = Route {
            sink: Some(sink.clone()),
            ..Default::default()
        };
        route.configure("fixed-uid".into(), false, false, false, Instant::now());
        for available in [None, Some(false)] {
            route.fixed_available(available);
            assert!(sink.take().is_none());
        }
        route.fixed_available(Some(true));
        let facts = sink.take().unwrap();
        assert_eq!(facts.route.route, Some(DeviceId("fixed-uid".into())));
        assert!(!facts.route.default_changed && !facts.route.headphones_disconnected);
        route.follows_default = true;
        route.fixed_available(Some(true));
        assert!(sink.take().is_none());
    }
}
