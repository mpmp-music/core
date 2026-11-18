//! The output boundary. Backends live in their own crates
//! (`output-cpal`, later exclusive-mode backends); the engine only sees these traits.

#[doc(hidden)]
pub mod policy;
#[doc(hidden)]
pub mod retry;

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossbeam_channel::Sender;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DeviceId(pub String);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    pub id: DeviceId,
    pub name: String,
    pub is_default: bool,
    /// False for identities that must not be restored in another core run.
    pub persistent: bool,
}

/// A staged discovery observation. Native handles remain private to this result
/// until core validates its settings revision and discovery epoch.
pub struct DiscoveredDevices {
    pub devices: Vec<DeviceInfo>,
    pub handles: Option<Box<dyn std::any::Any + Send>>,
}

impl From<Vec<DeviceInfo>> for DiscoveredDevices {
    fn from(devices: Vec<DeviceInfo>) -> Self {
        Self {
            devices,
            handles: None,
        }
    }
}

/// Independent driver work: cooperate with cancellation between native calls;
/// never retain a core sender or a strong reference to the live backend cache.
pub type DeviceDiscovery = Arc<
    dyn Fn(crate::output_task::Cancellation) -> Result<DiscoveredDevices, OutputError>
        + Send
        + Sync,
>;

pub type CapabilityQuery =
    Box<dyn FnOnce(crate::output_task::Cancellation) -> Result<OutputCaps, OutputError> + Send>;

/// Stream identity confirmed by the backend's native route observer. A pinned
/// stream retains its initial identity until it is actually reopened.
#[derive(Debug, Clone)]
pub struct OutputIdentity(Arc<Mutex<Option<DeviceInfo>>>);

impl OutputIdentity {
    pub fn new(device: DeviceInfo) -> Self {
        Self(Arc::new(Mutex::new(Some(device))))
    }
    pub fn get(&self) -> Option<DeviceInfo> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
    pub fn set(&self, device: Option<DeviceInfo>) -> bool {
        let mut current = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if *current == device {
            return false;
        }
        *current = device;
        true
    }
}

/// Sample types a [`Renderer`] can write natively.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleType {
    F32,
    I16,
    I32,
}

impl SampleType {
    pub fn bits(self) -> u32 {
        match self {
            SampleType::F32 | SampleType::I32 => 32,
            SampleType::I16 => 16,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamFormat {
    pub sample_rate: u32,
    pub channels: u16,
    pub sample_type: SampleType,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShareMode {
    Shared,
    Exclusive,
}

#[derive(Debug, Clone)]
pub struct OutputCaps {
    /// The format the OS mixer runs at; the engine opens shared streams with it so that
    /// the OS never resamples behind our back.
    pub mix_format: StreamFormat,
    pub supports_exclusive: bool,
}

#[derive(Debug, Clone)]
pub struct StreamRequest {
    /// `None` selects the system default device.
    pub device: Option<DeviceId>,
    pub format: StreamFormat,
    pub share_mode: ShareMode,
    /// Where the backend reports device events. Backends must never block on it.
    pub events: OutputEventSink,
}

/// Device-level events, carrying the reason so that core can decide what to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum OutputEvent {
    /// Only from backends that can tell an unplug apart from other losses; cpal cannot,
    /// and reports [`OutputEvent::DeviceUnavailable`] instead.
    DeviceUnplugged,
    DeviceUnavailable,
    Rerouted,
    StreamInvalidated,
    Xrun,
    Unknown,
    /// A callback timestamp exceeded the engine's buffered-audio timing budget.
    ClockUnreliable,
}

/// Native facts about the route of one output stream. Never serialized.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RouteFacts {
    pub devices_changed: bool,
    pub identity_changed: bool,
    pub route: Option<DeviceId>,
    pub default_changed: bool,
    pub old_route_available: Option<bool>,
    pub headphones_disconnected: bool,
    /// Native mix-format property changed on the current endpoint, without route loss.
    pub format_changed: bool,
    /// This route cannot migrate to the new default without reopening its stream.
    pub rebuild_required: bool,
}

#[derive(Debug, Clone)]
pub struct OutputFacts {
    pub generation: u64,
    pub route: RouteFacts,
    events: u32,
}

impl OutputFacts {
    pub(crate) fn clock_unreliable(generation: u64) -> Self {
        Self {
            generation,
            route: RouteFacts::default(),
            events: 1 << OutputEvent::ClockUnreliable as u32,
        }
    }

    pub(crate) fn mark_clock_unreliable(&mut self) {
        self.events |= 1 << OutputEvent::ClockUnreliable as u32;
    }
    pub(crate) fn disconnect(generation: u64) -> Self {
        Self {
            generation,
            route: RouteFacts {
                headphones_disconnected: true,
                ..Default::default()
            },
            events: 0,
        }
    }

    pub fn contains(&self, event: OutputEvent) -> bool {
        self.events & (1 << event as u32) != 0
    }
}

#[derive(Debug)]
struct PendingOutput {
    events: AtomicU32,
    route: Mutex<RouteFacts>,
}

/// Each stream owns its error latch and shares a persistent disconnect conclusion.
/// Retired callbacks are ignored; a full wake queue cannot lose device facts.
#[derive(Debug, Clone)]
pub struct OutputEventSink {
    generation: u64,
    pending: Arc<PendingOutput>,
    wake: Sender<()>,
    disconnected: Arc<AtomicBool>,
    retired: Arc<AtomicBool>,
}

impl OutputEventSink {
    pub fn new(generation: u64, wake: Sender<()>) -> Self {
        Self {
            generation,
            pending: Arc::new(PendingOutput {
                events: AtomicU32::new(0),
                route: Mutex::new(RouteFacts::default()),
            }),
            wake,
            disconnected: Arc::new(AtomicBool::new(false)),
            retired: Arc::new(AtomicBool::new(false)),
        }
    }

    /// A stream-local error latch sharing the persistent disconnect conclusion.
    pub fn stream(&self, generation: u64) -> Self {
        let mut sink = Self::new(generation, self.wake.clone());
        sink.disconnected = Arc::clone(&self.disconnected);
        sink
    }

    pub(crate) fn retire(&self) {
        self.retired.store(true, Ordering::Release);
    }

    pub fn take_disconnect(&self) -> bool {
        self.disconnected.swap(false, Ordering::AcqRel)
    }

    /// Safe on the audio error callback: only atomic operations, no channel wake.
    pub fn report(&self, event: OutputEvent) {
        if self.retired.load(Ordering::Acquire) {
            return;
        }
        self.pending
            .events
            .fetch_or(1 << event as u32, Ordering::Release);
        if event == OutputEvent::DeviceUnplugged {
            self.disconnected.store(true, Ordering::Release);
        }
    }

    /// Called by device workers, never by native callbacks or the renderer.
    pub fn report_route(&self, facts: RouteFacts) {
        if self.retired.load(Ordering::Acquire) {
            return;
        }
        if facts.headphones_disconnected {
            self.disconnected.store(true, Ordering::Release);
        }
        let mut pending = self.pending.route.lock().unwrap_or_else(|e| e.into_inner());
        if facts.route.is_some() {
            pending.route = facts.route;
        }
        pending.devices_changed |= facts.devices_changed;
        pending.identity_changed |= facts.identity_changed;
        pending.default_changed |= facts.default_changed;
        pending.headphones_disconnected |= facts.headphones_disconnected;
        pending.format_changed |= facts.format_changed;
        pending.rebuild_required |= facts.rebuild_required;
        // Loss evidence wins over an out-of-order default-device notification.
        pending.old_route_available = match (pending.old_route_available, facts.old_route_available)
        {
            (Some(false), _) | (_, Some(false)) => Some(false),
            (a, b) => b.or(a),
        };
        drop(pending);
        let _ = self.wake.try_send(());
    }

    pub fn take(&self) -> Option<OutputFacts> {
        let route =
            std::mem::take(&mut *self.pending.route.lock().unwrap_or_else(|e| e.into_inner()));
        let events = self.pending.events.swap(0, Ordering::AcqRel);
        (events != 0 || route != RouteFacts::default()).then_some(OutputFacts {
            generation: self.generation,
            route,
            events,
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum OutputError {
    #[error("no output device available")]
    NoDevice,
    #[error("unsupported stream format: {0}")]
    UnsupportedFormat(String),
    #[error("output backend error: {0}")]
    Backend(String),
}

/// Interleaved output buffer in the device's native sample type.
pub enum SampleBufMut<'a> {
    F32(&'a mut [f32]),
    I16(&'a mut [i16]),
    I32(&'a mut [i32]),
}

impl SampleBufMut<'_> {
    pub fn len(&self) -> usize {
        match self {
            SampleBufMut::F32(b) => b.len(),
            SampleBufMut::I16(b) => b.len(),
            SampleBufMut::I32(b) => b.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct RenderInfo {
    /// How long until the first frame of this buffer is heard, when the backend knows.
    pub playback_delay: Option<Duration>,
}

/// Implemented by the engine; called by backends on the realtime audio thread, so
/// implementations must not allocate, lock, log or do I/O.
pub trait Renderer: Send + 'static {
    fn render(&mut self, out: SampleBufMut<'_>, info: &RenderInfo);
}

/// An open stream. Dropping it closes the stream.
pub trait OutputStream: Send {
    /// Identity bound by the opened stream, independent of the requested default.
    fn device_info(&self) -> Option<DeviceInfo> {
        None
    }
    fn format(&self) -> StreamFormat;
}

pub trait OutputBackend: Send {
    /// Actor-only commit after validating the public list and observation token.
    fn commit_discovery(&mut self, _result: DiscoveredDevices) {}
    /// Fast factory for an independent, read-only operation. It must not perform
    /// native capability reads or change the preparation used by open().
    fn capability_query(&self, _device: Option<&DeviceId>) -> Result<CapabilityQuery, OutputError> {
        Err(OutputError::Backend(
            "independent capability reads unavailable".into(),
        ))
    }
    fn device_discovery(&self) -> Option<DeviceDiscovery> {
        None
    }
    /// True only when the running observer covers device list changes as well as routes.
    fn has_native_notifications(&self) -> bool {
        false
    }
    /// Keeps device arrival notifications available even before the first stream opens.
    /// This sink stays valid across stream replacement and failed open attempts.
    fn watch_output(&mut self, _events: OutputEventSink) -> Result<(), OutputError> {
        Ok(())
    }

    /// Stability of a previously known identity, without resolving/opening it.
    fn identity_persistent(&self, _device: &DeviceId) -> bool {
        false
    }

    /// Records a user-selected identity before its first capability/open attempt.
    fn select_device(&mut self, _device: Option<&DeviceId>) -> Result<(), OutputError> {
        Ok(())
    }
    fn devices(&self) -> Result<Vec<DeviceInfo>, OutputError>;
    fn capabilities(&self, device: Option<&DeviceId>) -> Result<OutputCaps, OutputError>;
    fn open(
        &mut self,
        request: StreamRequest,
        renderer: Box<dyn Renderer>,
    ) -> Result<Box<dyn OutputStream>, OutputError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_reports_never_wake_a_channel_waiter() {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let sink = OutputEventSink::new(1, tx);
        sink.report(OutputEvent::DeviceUnplugged);
        sink.report(OutputEvent::StreamInvalidated);
        assert!(rx.is_empty());
        assert!(sink.take_disconnect());
        let facts = sink.take().unwrap();
        assert!(facts.contains(OutputEvent::DeviceUnplugged));
        assert!(facts.contains(OutputEvent::StreamInvalidated));
    }

    #[test]
    fn disconnect_conclusion_crosses_replacement_but_retired_reports_do_not() {
        let (tx, _rx) = crossbeam_channel::bounded(1);
        let devices = OutputEventSink::new(0, tx);
        let old = devices.stream(1);
        old.report(OutputEvent::DeviceUnplugged);
        old.retire();
        let new = devices.stream(2);
        assert!(new.take_disconnect());
        old.report(OutputEvent::DeviceUnplugged);
        assert!(!new.take_disconnect());
        devices.report_route(RouteFacts {
            headphones_disconnected: true,
            ..Default::default()
        });
        assert!(new.take_disconnect());
    }

    #[test]
    fn full_wakeup_latches_loss_and_retired_streams_are_isolated() {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let old = OutputEventSink::new(1, tx.clone());
        let current = OutputEventSink::new(2, tx);
        old.report(OutputEvent::Xrun);
        current.report_route(RouteFacts {
            headphones_disconnected: true,
            old_route_available: Some(false),
            ..Default::default()
        });
        for _ in 0..1000 {
            current.report(OutputEvent::StreamInvalidated);
        }
        current.report_route(RouteFacts {
            default_changed: true,
            old_route_available: Some(true),
            ..Default::default()
        });
        assert_eq!(rx.len(), 1);
        let facts = current.take().unwrap();
        assert_eq!(facts.generation, 2);
        assert!(facts.route.headphones_disconnected);
        assert_eq!(facts.route.old_route_available, Some(false));
        assert!(facts.contains(OutputEvent::StreamInvalidated));
        assert!(!facts.contains(OutputEvent::Xrun));
        old.report(OutputEvent::DeviceUnplugged);
        assert!(current.take().is_none());
    }
}
