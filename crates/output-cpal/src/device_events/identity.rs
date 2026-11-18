//! Native workers update a stream's identity only after observing its actual route.
use mpmp_engine::output::{DeviceInfo, OutputEventSink, OutputIdentity, RouteFacts};

#[derive(Default)]
pub(crate) struct ActiveIdentity {
    stream: Option<OutputIdentity>,
    automatic: bool,
}

impl ActiveIdentity {
    pub fn attach(&mut self, stream: Option<OutputIdentity>, automatic: bool) {
        self.stream = stream;
        self.automatic = automatic;
    }
    pub fn is_automatic(&self) -> bool {
        self.automatic && self.stream.is_some()
    }
    pub fn confirm(&self, device: Option<DeviceInfo>, sink: &OutputEventSink) {
        if self.automatic
            && let Some(stream) = &self.stream
            && stream.set(device)
        {
            sink.report_route(RouteFacts {
                identity_changed: true,
                ..Default::default()
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mpmp_engine::output::DeviceId;
    fn device(id: &str) -> DeviceInfo {
        DeviceInfo {
            id: DeviceId(id.into()),
            name: format!("Device {id}"),
            is_default: true,
            persistent: true,
        }
    }
    #[test]
    fn automatic_identity_moves_without_reopening_but_pinned_identity_waits_for_rebuild() {
        for automatic in [false, true] {
            let identity = OutputIdentity::new(device("A"));
            let (tx, _) = crossbeam_channel::bounded(1);
            let sink = OutputEventSink::new(1, tx);
            let mut active = ActiveIdentity::default();
            active.attach(Some(identity.clone()), automatic);
            assert_eq!(active.is_automatic(), automatic);
            active.confirm(Some(device("B")), &sink);
            assert_eq!(
                identity.get().unwrap().id.0,
                if automatic { "B" } else { "A" }
            );
            assert_eq!(sink.take().is_some(), automatic);
            active.attach(None, false);
            assert!(!active.is_automatic());
            active.confirm(Some(device("C")), &sink);
            assert!(sink.take().is_none());
        }
    }
}
