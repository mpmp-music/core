//! HAL listeners enqueue fixed-size observations. Queries and logging run on the worker.
use std::ffi::c_void;
use std::ptr::{NonNull, null};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{Sender, bounded, select, tick};
use crossbeam_queue::ArrayQueue;
use mpmp_engine::output::{OutputError, OutputEventSink, OutputIdentity, RouteFacts};
use objc2_core_audio::*;
use objc2_core_foundation::{CFRetained, CFString};

use super::macos_route::{
    FixedChange, Properties, default_transition, find_uid, fixed_change, fixed_facts, publish,
    take_watched, transition,
};

#[derive(Debug, Clone, Copy)]
struct Notice {
    object: u32,
    selector: u32,
    sequence: u64,
}

struct Callback {
    accepting: AtomicBool,
    notices: ArrayQueue<Notice>,
    overflow: AtomicBool,
    sequence: AtomicU64,
    errors: ArrayQueue<(cpal::ErrorKind, u64)>,
    errors_dropped: AtomicU64,
}

#[derive(Clone)]
pub(crate) struct StreamErrors(Arc<Callback>);

impl StreamErrors {
    pub fn record(&self, kind: cpal::ErrorKind) {
        if !self.0.accepting.load(Ordering::Acquire) {
            return;
        }
        let sequence = self.0.sequence.fetch_add(1, Ordering::Relaxed);
        if self.0.errors.push((kind, sequence)).is_err() {
            self.0.errors_dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

// SAFETY: CoreAudio owns the call; client points to an Arc allocation retained
// for process lifetime (including failed removals and already-entered callbacks).
// No query, allocation, lock or log here.
unsafe extern "C-unwind" fn changed(
    object: u32,
    count: u32,
    addresses: NonNull<AudioObjectPropertyAddress>,
    client: *mut c_void,
) -> i32 {
    if client.is_null() {
        return 0;
    }
    let state = unsafe { &*client.cast::<Callback>() };
    if !state.accepting.load(Ordering::Acquire) {
        return 0;
    }
    for index in 0..count as usize {
        let address = unsafe { &*addresses.as_ptr().add(index) };
        let notice = Notice {
            object,
            selector: address.mSelector,
            sequence: state.sequence.fetch_add(1, Ordering::Relaxed),
        };
        if state.notices.push(notice).is_err() {
            state.overflow.store(true, Ordering::Release);
        }
    }
    0
}

#[allow(non_upper_case_globals)] // Match Apple's public selector constants.
fn property_name(selector: u32) -> &'static str {
    match selector {
        kAudioHardwarePropertyDefaultOutputDevice => "default_output",
        kAudioHardwarePropertyDevices => "devices",
        kAudioDevicePropertyDeviceIsAlive => "device_alive",
        kAudioDevicePropertyJackIsConnected => "jack_connected",
        kAudioDevicePropertyDataSource => "data_source",
        kAudioDevicePropertyNominalSampleRate => "sample_rate",
        kAudioDevicePropertyStreamConfiguration => "stream_configuration",
        _ => "other",
    }
}

fn address(selector: u32, scope: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: scope,
        mElement: kAudioObjectPropertyElementMain,
    }
}

fn read<T: Copy + Default>(object: u32, selector: u32, scope: u32) -> Option<T> {
    let property = address(selector, scope);
    let mut value = T::default();
    let mut size = size_of::<T>() as u32;
    let result = unsafe {
        AudioObjectGetPropertyData(
            object,
            NonNull::from(&property),
            0,
            null(),
            NonNull::from(&mut size),
            NonNull::from(&mut value).cast(),
        )
    };
    (result == 0 && size as usize == size_of::<T>()).then_some(value)
}

fn properties(object: u32) -> Properties {
    Properties {
        alive: read::<u32>(
            object,
            kAudioDevicePropertyDeviceIsAlive,
            kAudioObjectPropertyScopeGlobal,
        )
        .map(|v| v != 0),
        jack: read::<u32>(
            object,
            kAudioDevicePropertyJackIsConnected,
            kAudioObjectPropertyScopeOutput,
        )
        .map(|v| v != 0),
        source: read(
            object,
            kAudioDevicePropertyDataSource,
            kAudioObjectPropertyScopeOutput,
        ),
    }
}

fn default_device() -> Option<u32> {
    read(
        kAudioObjectSystemObject as u32,
        kAudioHardwarePropertyDefaultOutputDevice,
        kAudioObjectPropertyScopeGlobal,
    )
    .filter(|id| *id != 0)
}

fn uid(object: u32) -> Option<String> {
    let pointer: *mut CFString = read(
        object,
        kAudioDevicePropertyDeviceUID,
        kAudioObjectPropertyScopeGlobal,
    )?;
    // DeviceUID uses the CoreAudio create rule, just as CPAL's DeviceTrait::id().
    let uid = unsafe { CFRetained::from_raw(NonNull::new(pointer)?) };
    Some(cpal::DeviceId::new(cpal::HostId::CoreAudio, uid.to_string()).to_string())
}

fn find_device(id: &str) -> Option<u32> {
    let id_value = id.parse::<cpal::DeviceId>().ok()?;
    if id_value.host() != cpal::HostId::CoreAudio {
        return None;
    }
    let value = CFString::from_str(id_value.id());
    let pointer: *const CFString = &*value;
    let property = address(
        kAudioHardwarePropertyTranslateUIDToDevice,
        kAudioObjectPropertyScopeGlobal,
    );
    let mut object = 0_u32;
    let mut size = size_of::<u32>() as u32;
    // Translate the selected UID directly. Enumerating all UIDs here would block
    // the actor waiting for output registration, even though it runs on a worker.
    let result = unsafe {
        AudioObjectGetPropertyData(
            kAudioObjectSystemObject as u32,
            NonNull::from(&property),
            size_of::<*const CFString>() as u32,
            (&pointer as *const *const CFString).cast(),
            NonNull::from(&mut size),
            NonNull::from(&mut object).cast(),
        )
    };
    if result != 0 || size as usize != size_of::<u32>() || object == 0 {
        return None;
    }
    find_uid(id, [(object, uid(object))])
}

struct CallbackLease(Arc<Callback>);

impl Drop for CallbackLease {
    fn drop(&mut self) {
        self.0.accepting.store(false, Ordering::Release);
    }
}

struct Listener {
    object: u32,
    property: AudioObjectPropertyAddress,
    callback: Arc<Callback>,
}

impl Listener {
    fn new(object: u32, selector: u32, scope: u32, callback: &Arc<Callback>) -> Option<Self> {
        let property = address(selector, scope);
        if !unsafe { AudioObjectHasProperty(object, NonNull::from(&property)) } {
            tracing::debug!(object, selector, scope, "macOS route property unsupported");
            return None;
        }
        let result = unsafe {
            AudioObjectAddPropertyListener(
                object,
                NonNull::from(&property),
                Some(changed),
                Arc::as_ptr(callback).cast_mut().cast(),
            )
        };
        if result != 0 {
            tracing::warn!(
                object,
                selector,
                result,
                "macOS route listener registration failed"
            );
            return None;
        }
        tracing::debug!(object, selector, scope, "macOS route listener registered");
        Some(Self {
            object,
            property,
            callback: Arc::clone(callback),
        })
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        let result = unsafe {
            AudioObjectRemovePropertyListener(
                self.object,
                NonNull::from(&self.property),
                Some(changed),
                Arc::as_ptr(&self.callback).cast_mut().cast(),
            )
        };
        // The monitor's process-lifetime reference keeps clientData valid.
        if result == kAudioHardwareBadObjectError {
            tracing::debug!(
                object = self.object,
                selector = self.property.mSelector,
                result,
                "macOS listener removed after device disappeared; callback storage retained"
            );
        } else if result != 0 {
            tracing::warn!(
                object = self.object,
                selector = self.property.mSelector,
                result,
                "macOS listener removal failed; callback storage retained"
            );
        }
    }
}

struct Device {
    object: u32,
    id: String,
    properties: Properties,
    _listeners: Vec<Listener>,
}

impl Device {
    fn new(object: u32, callback: &Arc<Callback>) -> Option<Self> {
        let id = uid(object)?;
        let mut listeners = Vec::new();
        for (selector, scope) in [
            (
                kAudioDevicePropertyDeviceIsAlive,
                kAudioObjectPropertyScopeGlobal,
            ),
            (
                kAudioDevicePropertyJackIsConnected,
                kAudioObjectPropertyScopeOutput,
            ),
            (
                kAudioDevicePropertyDataSource,
                kAudioObjectPropertyScopeOutput,
            ),
            (
                kAudioDevicePropertyNominalSampleRate,
                kAudioObjectPropertyScopeGlobal,
            ),
            (
                kAudioDevicePropertyStreamConfiguration,
                kAudioObjectPropertyScopeOutput,
            ),
        ] {
            if let Some(listener) = Listener::new(object, selector, scope, callback) {
                listeners.push(listener);
            }
        }
        let properties = properties(object);
        tracing::debug!(object, %id, ?properties, "macOS route baseline");
        Some(Self {
            object,
            id,
            properties,
            _listeners: listeners,
        })
    }
}

#[derive(Default)]
struct Route {
    sink: Option<OutputEventSink>,
    identity: super::identity::ActiveIdentity,
    current: Option<Device>,
    previous: Vec<(Device, Instant)>,
    follows_default: bool,
    fixed_id: Option<String>,
    devices_listener: Option<Listener>,
}

impl Route {
    fn replace(&mut self, device: Option<Device>) {
        if let Some(old) = self.current.take() {
            self.previous.push((old, Instant::now()));
        }
        self.current = device;
    }
    /// HAL refuses a second registration of the same listener, and dropping the first
    /// would then remove it: a device already watched keeps its listeners.
    fn device(&mut self, object: u32, callback: &Arc<Callback>) -> Option<Device> {
        match take_watched(&mut self.current, &mut self.previous, object, |d| d.object) {
            Some(device) => {
                // Do not query here: queued notifications still need the old baseline.
                tracing::debug!(object,
                 id = %device.id,
                 properties = ?device.properties,
                 "macOS route baseline");

                Some(device)
            }
            None => Device::new(object, callback),
        }
    }
    fn refresh_fixed(&mut self, callback: &Arc<Callback>) {
        let Some(id) = self.fixed_id.clone() else {
            return;
        };
        let current = self.current.as_ref().map(|d| d.object);
        let found = find_device(&id);
        let Some(change) = fixed_change(self.follows_default, current, found) else {
            return;
        };
        let old = self
            .current
            .as_ref()
            .map(|d| d.properties)
            .unwrap_or_default();
        tracing::debug!(%id, ?current, ?found, ?change, "macOS fixed device list changed");
        let device = match change {
            FixedChange::Missing => None,
            FixedChange::Reconnected(object) => {
                let Some(device) = self.device(object, callback).filter(|d| d.id == id) else {
                    tracing::debug!(%id, object, "macOS fixed device not ready for listeners");
                    return;
                };
                Some(device)
            }
        };
        self.replace(device);
        if let Some(sink) = &self.sink {
            publish(sink, Some(id), fixed_facts(change, old), true);
        }
    }

    fn default_notice(&mut self, callback: &Arc<Callback>, synthetic: bool) {
        if let Some(sink) = &self.sink {
            super::macos_route::default_inventory_changed(sink);
        }
        if !self.follows_default {
            return;
        }
        let Some(sink) = self.sink.clone() else {
            return;
        };
        let new = default_device();
        let current = self.current.as_ref().map(|d| d.object);
        tracing::debug!(?current, ?new, synthetic, "macOS default route observed");
        if new == current {
            // A same-object notice can mean a different physical route even when
            // the sampled attributes match. Never absorb it as a duplicate.
            if let Some(device) = &mut self.current {
                let observed = properties(device.object);
                let facts = transition(device.properties, observed, true);
                publish(&sink, Some(device.id.clone()), facts, true);
                device.properties = observed;
            } else {
                publish(&sink, None, RouteFacts::default(), true);
            }
            return;
        }
        let available = self
            .current
            .as_ref()
            .and_then(|d| properties(d.object).available_since(d.properties));
        let device = new.and_then(|id| self.device(id, callback));
        publish(
            &sink,
            device.as_ref().map(|d| d.id.clone()),
            default_transition(available),
            false,
        );
        self.replace(device);
        self.confirm_identity();
    }

    fn confirm_identity(&self) {
        if self.identity.is_automatic()
            && let Some(sink) = &self.sink
        {
            let observed = self
                .current
                .as_ref()
                .and_then(|d| d.id.parse::<cpal::DeviceId>().ok())
                .and_then(|id| crate::confirmed_default_identity(&id));
            self.identity.confirm(observed, sink);
        }
    }

    fn notice(&mut self, notice: Notice, callback: &Arc<Callback>) {
        let _timing = crate::macos_timing::Timing::start("route_notice", "total", notice.sequence);
        tracing::debug!(
            sequence = notice.sequence,
            object = notice.object,
            selector = notice.selector,
            kind = property_name(notice.selector),
            "macOS route callback dequeued"
        );
        let Some(sink) = self.sink.clone() else {
            return;
        };
        if notice.selector == kAudioHardwarePropertyDevices {
            sink.report_route(RouteFacts {
                devices_changed: true,
                ..Default::default()
            });
            if !self.follows_default {
                self.refresh_fixed(callback);
            }
            return;
        }
        if notice.selector == kAudioHardwarePropertyDefaultOutputDevice {
            self.default_notice(callback, false);
            return;
        }
        let current_id = self.current.as_ref().map(|device| device.id.clone());
        let device = if self
            .current
            .as_ref()
            .is_some_and(|d| d.object == notice.object)
        {
            self.current.as_mut()
        } else {
            self.previous
                .iter_mut()
                .find(|(d, at)| {
                    d.object == notice.object && at.elapsed() <= Duration::from_millis(300)
                })
                .map(|(d, _)| d)
        };
        let Some(device) = device else {
            return;
        };
        if notice.selector == kAudioDevicePropertyNominalSampleRate
            || notice.selector == kAudioDevicePropertyStreamConfiguration
        {
            if self
                .current
                .as_ref()
                .is_some_and(|d| d.object == notice.object)
            {
                publish(
                    &sink,
                    None,
                    RouteFacts {
                        format_changed: true,
                        ..Default::default()
                    },
                    false,
                );
            }
            return;
        }
        let new = properties(device.object);
        let source = notice.selector == kAudioDevicePropertyDataSource;
        let facts = transition(device.properties, new, source);
        let unknown = source || facts.old_route_available != Some(true);
        tracing::debug!(object = device.object,
         old = ?device.properties,
         ?new,
         source,
         "macOS route properties observed");

        publish(&sink, current_id, facts, unknown);
        device.properties = new;
    }
}

enum Control {
    Watch(OutputEventSink),
    Output {
        id: String,
        follows_default: bool,
        identity: Option<OutputIdentity>,
        done: Sender<Result<(), OutputError>>,
    },
}

pub(crate) struct DeviceMonitor {
    errors: StreamErrors,
    control: Sender<Control>,
    stop: Sender<()>,
    thread: Option<JoinHandle<()>>,
}

impl DeviceMonitor {
    pub fn start() -> Result<Self, OutputError> {
        let (control, controls) = bounded(64);
        let (stop, stopping) = bounded(1);
        let (ready, started) = bounded(1);
        let callback = Arc::new(Callback {
            accepting: AtomicBool::new(true),
            notices: ArrayQueue::new(256),
            overflow: AtomicBool::new(false),
            sequence: AtomicU64::new(0),
            errors: ArrayQueue::new(64),
            errors_dropped: AtomicU64::new(0),
        });
        let errors = StreamErrors(Arc::clone(&callback));
        let thread = std::thread::Builder::new()
            .name("coreaudio-routes".into())
            .spawn(move || {
                // HAL's remove API does not document an in-flight callback barrier.
                // Retain one bounded POD queue per monitor until process exit; never
                // retain sinks, streams, listeners or threads through this reference.
                std::mem::forget(Arc::clone(&callback));
                let _lease = CallbackLease(Arc::clone(&callback));
                let Some(_default) = Listener::new(
                    kAudioObjectSystemObject as u32,
                    kAudioHardwarePropertyDefaultOutputDevice,
                    kAudioObjectPropertyScopeGlobal,
                    &callback,
                ) else {
                    let _ = ready.send(Err(OutputError::Backend(
                        concat!("CoreAudio default output listener ", "unavailable",).into(),
                    )));
                    return;
                };
                let Some(devices_listener) = Listener::new(
                    kAudioObjectSystemObject as u32,
                    kAudioHardwarePropertyDevices,
                    kAudioObjectPropertyScopeGlobal,
                    &callback,
                ) else {
                    let _ = ready.send(Err(OutputError::Backend(
                        concat!("CoreAudio device list listener ", "unavailable",).into(),
                    )));
                    return;
                };
                let mut route = Route {
                    devices_listener: Some(devices_listener),
                    follows_default: true,
                    current: default_device().and_then(|id| Device::new(id, &callback)),
                    ..Default::default()
                };

                let _ = ready.send(Ok(()));
                let ticker = tick(Duration::from_millis(25));
                loop {
                    select! {
                        recv(stopping) -> _ => {
                            break;
                        },
                        recv(controls) -> control => {
                            match control {
                                Ok(Control::Watch(sink)) => route.sink = Some(sink),
                                Ok(Control::Output {
                                    id,
                                    follows_default,
                                    identity,
                                    done,
                                }) => {
                                    // Process observations for the earlier sink before publishing a new baseline.
                                    for _ in 0..256 {
                                        let Some(notice) = callback.notices.pop() else {
                                            break;
                                        };
                                        route.notice(notice, &callback);
                                    }

                                    route.identity.attach(identity, follows_default);
                                    if follows_default {
                                        // Selection can return to default while no endpoint exists.
                                        // Clear the fixed UID before resolving, retaining the global
                                        // default listener so a later arrival is still observed.
                                        route.follows_default = true;
                                        route.fixed_id = None;
                                        if id.is_empty() {
                                            route.current = None;
                                            route.previous.clear();
                                            let _ = done.try_send(Ok(()));
                                            continue;
                                        }
                                    }
                                    if !follows_default && route.devices_listener.is_none() {
                                        route.devices_listener = Listener::new(
                                            kAudioObjectSystemObject as u32,
                                            kAudioHardwarePropertyDevices,
                                            kAudioObjectPropertyScopeGlobal,
                                            &callback,
                                        );

                                        if route.devices_listener.is_none() {
                                            let _ = done.try_send(Err(OutputError::Backend(
                                                concat!(
                                                    "CoreAudio device list listener ",
                                                    "unavailable",
                                                )
                                                .into(),
                                            )));

                                            continue;
                                        }
                                    }
                                    if !follows_default {
                                        // Retain the requested UID even if it vanishes during setup.
                                        route.fixed_id = Some(id.clone());
                                        route.follows_default = false;

                                        if route.current.as_ref().is_some_and(|d| d.id != id) {
                                            route.current = None;
                                        }

                                        route.previous.retain(|(d, _)| d.id == id);
                                    }
                                    let device = find_device(&id)
                                        .and_then(|object| route.device(object, &callback));

                                    if let Some(device) = device {
                                        route.current = Some(device);
                                        route.previous.clear();
                                        route.follows_default = follows_default;
                                        route.fixed_id = (!follows_default).then_some(id);
                                        let _ = done.try_send(Ok(()));
                                        if follows_default {
                                            let current =
                                                route.current.as_ref().map(|d| d.object);
                                            let default = default_device();
                                            if default != current {
                                                route.default_notice(&callback, true);
                                            }
                                        }
                                    } else {
                                        let _ = done.try_send(Err(OutputError::NoDevice));
                                    }
                                }
                                Err(_) => break,
                            }
                        },
                        recv(ticker) -> _ => {

                        },
                    }
                    for _ in 0..256 {
                        let Some(notice) = callback.notices.pop() else {
                            break;
                        };
                        route.notice(notice, &callback);
                    }

                    if callback.overflow.swap(false, Ordering::AcqRel)
                        && let Some(sink) = &route.sink
                    {
                        tracing::warn!("macOS route callback queue overflow; reason unknown");
                        publish(sink, None, RouteFacts::default(), true);
                        if route.follows_default {
                            route.default_notice(&callback, true);
                        } else {
                            route.refresh_fixed(&callback);
                        }
                    }
                    for _ in 0..64 {
                        let Some((kind, sequence)) = callback.errors.pop() else {
                            break;
                        };
                        tracing::debug!(?kind, sequence, "macOS cpal stream error");
                    }
                    let dropped = callback.errors_dropped.swap(0, Ordering::Relaxed);

                    if dropped != 0 {
                        tracing::debug!(dropped, "macOS cpal diagnostic queue overflow");
                    }

                    route
                        .previous
                        .retain(|(_, at)| at.elapsed() <= Duration::from_millis(300));
                }
                callback.accepting.store(false, Ordering::Release);
            })
            .map_err(|e| OutputError::Backend(e.to_string()))?;
        started
            .recv()
            .map_err(|e| OutputError::Backend(e.to_string()))??;
        Ok(Self {
            errors,
            control,
            stop,
            thread: Some(thread),
        })
    }
    pub fn stream_errors(&self) -> StreamErrors {
        self.errors.clone()
    }
    pub fn is_running(&self) -> bool {
        self.thread.as_ref().is_some_and(|t| !t.is_finished())
    }

    pub fn watch(&self, sink: OutputEventSink) {
        let _ = self.control.send(Control::Watch(sink));
    }
    pub fn output(
        &self,
        id: String,
        follows_default: bool,
        identity: Option<OutputIdentity>,
    ) -> Result<(), OutputError> {
        let (done, completed) = bounded(1);
        self.control
            .send(Control::Output {
                id,
                follows_default,
                identity,
                done,
            })
            .map_err(|e| OutputError::Backend(e.to_string()))?;
        completed
            .recv()
            .map_err(|e| OutputError::Backend(e.to_string()))?
    }
}

impl Drop for DeviceMonitor {
    fn drop(&mut self) {
        let _ = self.stop.try_send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
