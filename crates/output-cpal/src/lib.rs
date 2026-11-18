//! cpal output backend. Shared mode only; the stream lives on a
//! dedicated thread because `cpal::Stream` is not `Send` on every platform.

#[cfg(any(windows, target_os = "macos", test))]
mod device_events;
#[cfg(any(target_os = "macos", test))]
mod macos;
#[cfg(any(windows, test))]
mod renderers;
#[cfg(target_os = "macos")]
use macos::timing as macos_timing;
#[cfg(any(windows, test))]
mod owner;
#[cfg(any(windows, test))]
mod windows;
#[cfg(windows)]
use windows::activation as windows_fallback;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, Device, ErrorKind, SampleFormat, StreamConfig};
use crossbeam_channel::{Sender, bounded};
use mpmp_engine::output::{
    CapabilityQuery, DeviceDiscovery, DeviceId, DeviceInfo, DiscoveredDevices, OutputBackend,
    OutputCaps, OutputError, OutputEvent, OutputEventSink, OutputIdentity, OutputStream,
    RenderInfo, Renderer, SampleBufMut, SampleType, ShareMode, StreamFormat, StreamRequest,
};

pub struct CpalBackend {
    host: cpal::Host,
    devices: Arc<Mutex<HashMap<String, Device>>>,
    #[cfg(any(windows, target_os = "macos"))]
    monitor: Option<device_events::DeviceMonitor>,
    #[cfg(windows)]
    prepared: std::sync::Mutex<Option<windows_fallback::Prepared>>,
    #[cfg(windows)]
    owner: std::sync::Mutex<Option<std::sync::Arc<windows_fallback::AudioOwner>>>,
}

impl CpalBackend {
    /// Backend on the platform's default cpal host.
    pub fn new() -> Self {
        Self {
            host: cpal::default_host(),
            devices: Arc::default(),
            #[cfg(any(windows, target_os = "macos"))]
            monitor: None,
            #[cfg(windows)]
            prepared: std::sync::Mutex::new(None),
            #[cfg(windows)]
            owner: std::sync::Mutex::new(None),
        }
    }

    /// `None` is the system default; `Some` matches the device id string.
    fn resolve(&self, id: Option<&DeviceId>) -> Result<Device, OutputError> {
        match id {
            None => self.host.default_output_device(),
            Some(id) => self
                .devices
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&id.0)
                .cloned(),
        }
        .ok_or(OutputError::NoDevice)
    }
}

impl Default for CpalBackend {
    fn default() -> Self {
        Self::new()
    }
}

fn backend_err(e: cpal::Error) -> OutputError {
    #[cfg(windows)]
    {
        use windows::Win32::System::Com::{APTTYPE, APTTYPEQUALIFIER, CoGetApartmentType};
        // Read the failing caller's apartment without initializing or changing COM.
        // CPAL can also propagate activation errors after its COM guard succeeded.
        let mut apartment = APTTYPE::default();
        let mut qualifier = APTTYPEQUALIFIER::default();
        let status = unsafe { CoGetApartmentType(&mut apartment, &mut qualifier) };
        let caller = thread::current();
        tracing::warn!(
            error = %e,
            thread = caller.name().unwrap_or("unnamed"),
            thread_id = ?caller.id(),
            apartment_status = ?status,
            apartment = ?apartment,
            apartment_qualifier = ?qualifier,
            "cpal operation failed"
        );
    }
    OutputError::Backend(e.to_string())
}

/// Maps a sample type the device reports natively; `None` for formats we cannot write.
fn native_sample_type(f: SampleFormat) -> Option<SampleType> {
    match f {
        SampleFormat::F32 => Some(SampleType::F32),
        SampleFormat::I16 => Some(SampleType::I16),
        SampleFormat::I32 => Some(SampleType::I32),
        _ => None,
    }
}

/// Picks the best writable type from those on offer: F32, then I32, then I16.
fn pick_sample_type(offered: &[SampleFormat]) -> Option<SampleType> {
    [SampleType::F32, SampleType::I32, SampleType::I16]
        .into_iter()
        .find(|t| offered.iter().any(|f| native_sample_type(*f) == Some(*t)))
}

/// Maps a cpal stream error to the event core should see, if any.
fn event_for(kind: ErrorKind) -> Option<OutputEvent> {
    match kind {
        ErrorKind::DeviceChanged => Some(OutputEvent::Rerouted),
        ErrorKind::DeviceNotAvailable => Some(OutputEvent::DeviceUnavailable),
        ErrorKind::StreamInvalidated => Some(OutputEvent::StreamInvalidated),
        ErrorKind::Xrun => Some(OutputEvent::Xrun),
        _ => Some(OutputEvent::Unknown),
    }
}

#[cfg(any(target_os = "macos", test))]
fn event_for_macos(kind: cpal::ErrorKind, monitored: bool) -> Option<OutputEvent> {
    if monitored && kind == cpal::ErrorKind::DeviceChanged {
        None // Native route facts own this decision; CPAL still records diagnostics.
    } else {
        event_for(kind)
    }
}

impl OutputBackend for CpalBackend {
    fn commit_discovery(&mut self, result: DiscoveredDevices) {
        if let Some(handles) = result
            .handles
            .and_then(|h| h.downcast::<HashMap<String, Device>>().ok())
        {
            let previous = std::mem::replace(
                &mut *self.devices.lock().unwrap_or_else(|e| e.into_inner()),
                *handles,
            );
            drop(previous);
        }
    }
    fn device_discovery(&self) -> Option<DeviceDiscovery> {
        // No live cache or core channel is captured by an uninterruptible read.
        Some(Arc::new(move |cancel| {
            cancel.check()?;
            let host = cpal::default_host();
            let default = host.default_output_device().and_then(|d| d.id().ok());
            let mut devices = HashMap::new();
            let mut infos = Vec::new();
            for device in host.output_devices().map_err(backend_err)? {
                cancel.check()?;
                let id = device.id().map_err(backend_err)?;
                infos.push(device_info(&device, default.as_ref() == Some(&id))?);
                devices.insert(id.to_string(), device);
            }
            cancel.check()?;
            Ok(DiscoveredDevices {
                devices: infos,
                handles: Some(Box::new(devices)),
            })
        }))
    }
    fn capability_query(&self, device: Option<&DeviceId>) -> Result<CapabilityQuery, OutputError> {
        // Resolve cached fixed identities before queuing. The worker owns only this
        // independent device handle and cannot mutate stream preparation.
        let device = device.map(|id| self.resolve(Some(id))).transpose()?;
        Ok(Box::new(move |cancel| {
            cancel.check()?;
            let device = match device {
                Some(device) => device,
                None => cpal::default_host()
                    .default_output_device()
                    .ok_or(OutputError::NoDevice)?,
            };
            let caps = device_capabilities(&device)?;
            cancel.check()?;
            Ok(caps)
        }))
    }
    fn has_native_notifications(&self) -> bool {
        #[cfg(any(windows, target_os = "macos"))]
        {
            self.monitor.as_ref().is_some_and(|m| m.is_running())
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        {
            false
        }
    }
    fn watch_output(&mut self, events: OutputEventSink) -> Result<(), OutputError> {
        #[cfg(any(windows, target_os = "macos"))]
        {
            if self.monitor.is_none() {
                self.monitor = Some(device_events::DeviceMonitor::start()?);
            }
            if let Some(monitor) = &self.monitor {
                monitor.watch(events);
            }
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        let _ = events;
        Ok(())
    }

    fn identity_persistent(&self, device: &DeviceId) -> bool {
        device
            .0
            .parse::<cpal::DeviceId>()
            .is_ok_and(|id| device_id_persistent(&id))
    }

    fn select_device(&mut self, device: Option<&DeviceId>) -> Result<(), OutputError> {
        // Register the expected UID before resolve: arrival must work even when the
        // very first fixed-device capability query fails. Read-only queries must
        // never retarget this observer to an unrelated endpoint.
        #[cfg(target_os = "macos")]
        if let Some(monitor) = &self.monitor {
            if let Some(id) = device {
                monitor.output(id.0.clone(), false, None)?;
            } else {
                let default = self
                    .host
                    .default_output_device()
                    .map(|d| d.id().map(|id| id.to_string()))
                    .transpose()
                    .map_err(backend_err)?;
                monitor.output(default.unwrap_or_default(), true, None)?;
            }
        }
        #[cfg(windows)]
        if let Some(monitor) = &self.monitor {
            if let Some(id) = device.and_then(|id| id.0.parse::<cpal::DeviceId>().ok()) {
                monitor.output(id.id().to_owned(), false, false, true, None);
            } else if device.is_none() {
                let endpoint = self
                    .host
                    .default_output_device()
                    .and_then(|d| d.id().ok())
                    .map(|id| id.id().to_owned())
                    .unwrap_or_default();
                monitor.output(endpoint, true, true, true, None);
            }
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        let _ = device;
        Ok(())
    }

    fn devices(&self) -> Result<Vec<DeviceInfo>, OutputError> {
        Ok(
            self.device_discovery().unwrap()(mpmp_engine::output_task::Cancellation::default())?
                .devices,
        )
    }

    fn capabilities(&self, device: Option<&DeviceId>) -> Result<OutputCaps, OutputError> {
        #[cfg(windows)]
        {
            *self.prepared.lock().unwrap_or_else(|e| e.into_inner()) = None;
            let preferred = self.owner.lock().unwrap_or_else(|e| e.into_inner()).clone();
            let devices = Arc::downgrade(&self.devices);
            let id = device.cloned();
            let task = mpmp_engine::output_task::OutputTask::start(move |cancel| {
                cancel.check()?;
                let result =
                    windows_fallback::capabilities_read(id, devices, preferred, cancel.clone())?;
                cancel.check()?;
                Ok(result)
            })?;
            let (caps, prepared) = task.wait(mpmp_engine::output_task::READ_TIMEOUT)?;
            // Only the bounded caller may install stream preparation. A late
            // worker result has no reference to this slot or to the live cache.
            *self.prepared.lock().unwrap_or_else(|e| e.into_inner()) = Some(prepared);
            Ok(caps)
        }
        #[cfg(not(windows))]
        {
            let query = self.capability_query(device)?;
            mpmp_engine::output_task::OutputTask::start(query)?
                .wait(mpmp_engine::output_task::READ_TIMEOUT)
        }
    }

    fn open(
        &mut self,
        request: StreamRequest,
        renderer: Box<dyn Renderer>,
    ) -> Result<Box<dyn OutputStream>, OutputError> {
        #[cfg(target_os = "macos")]
        let timing = macos_timing::Timing::start("open", "total", macos_timing::next_id());
        if request.share_mode != ShareMode::Shared {
            return Err(OutputError::UnsupportedFormat(
                "exclusive mode is not supported".into(),
            ));
        }
        #[cfg(windows)]
        {
            self.windows_open(request, renderer)
        }
        #[cfg(not(windows))]
        {
            let device = {
                #[cfg(target_os = "macos")]
                let _timing = macos_timing::Timing::start("open", "resolve", timing.id());
                self.resolve(request.device.as_ref())?
            };
            let identity = OutputIdentity::new(device_info(&device, request.device.is_none())?);
            #[cfg(target_os = "macos")]
            if let Some(monitor) = &self.monitor {
                let _watch = macos_timing::Timing::start("open", "route_watch", timing.id());
                monitor.output(
                    device.id().map_err(backend_err)?.to_string(),
                    request.device.is_none(),
                    Some(identity.clone()),
                )?;
            }
            spawn_output(
                device,
                request.format,
                identity,
                renderer,
                request.events,
                #[cfg(target_os = "macos")]
                self.monitor
                    .as_ref()
                    .map(device_events::DeviceMonitor::stream_errors),
                #[cfg(target_os = "macos")]
                timing.id(),
            )
        }
    }
}

// ALSA strings identify logical devices. Other Linux audio services may expose
// session objects; conservatively expire Linux IDs instead of guessing physical routes.
fn device_id_persistent(id: &cpal::DeviceId) -> bool {
    // ALSA device strings are persistent logical identities, not proof of a
    // physical route. Service object identities expire with the core session.
    cfg!(any(windows, target_os = "macos")) || id.host().name() == "ALSA"
}

#[cfg(any(windows, target_os = "macos"))]
fn confirmed_default_identity(expected: &cpal::DeviceId) -> Option<DeviceInfo> {
    let device = cpal::default_host().default_output_device()?;
    let info = device_info(&device, true).ok()?;
    // Windows default handles resolve dynamically. Recheck after reading the
    // description, refusing a newer default than the native observation.
    (info.id.0 == expected.to_string() && device.id().ok().as_ref() == Some(expected))
        .then_some(info)
}

fn device_info(device: &Device, is_default: bool) -> Result<DeviceInfo, OutputError> {
    let id = device.id().map_err(backend_err)?;
    Ok(DeviceInfo {
        id: DeviceId(id.to_string()),
        name: device
            .description()
            .map_err(backend_err)?
            .name()
            .to_string(),
        is_default,
        persistent: device_id_persistent(&id),
    })
}

fn device_capabilities(device: &Device) -> Result<OutputCaps, OutputError> {
    let cfg = device.default_output_config().map_err(backend_err)?;
    let (rate, channels) = (cfg.sample_rate(), cfg.channels());
    let sample_type = match native_sample_type(cfg.sample_format()) {
        Some(t) => t,
        None => {
            let offered: Vec<_> = device
                .supported_output_configs()
                .map_err(backend_err)?
                .filter(|r| r.channels() == channels && r.contains_rate(rate))
                .map(|r| r.sample_format())
                .collect();
            pick_sample_type(&offered).ok_or_else(|| {
                OutputError::UnsupportedFormat(format!(
                    "no F32/I32/I16 output at {rate} Hz x {channels} (default is {})",
                    cfg.sample_format()
                ))
            })?
        }
    };
    Ok(OutputCaps {
        mix_format: StreamFormat {
            sample_rate: rate,
            channels,
            sample_type,
        },
        supports_exclusive: false,
    })
}

fn spawn_output(
    device: Device,
    format: StreamFormat,
    identity: OutputIdentity,
    renderer: Box<dyn Renderer>,
    events: OutputEventSink,
    #[cfg(target_os = "macos")] errors: Option<device_events::StreamErrors>,
    #[cfg(target_os = "macos")] stream_id: u64,
) -> Result<Box<dyn OutputStream>, OutputError> {
    let (ready_tx, ready_rx) = bounded(1);
    let (close_tx, close_rx) = bounded::<()>(1);
    let thread = thread::Builder::new()
        .name("cpal-output".into())
        .spawn(move || {
            match build_stream(
                &device,
                format,
                renderer,
                events,
                #[cfg(target_os = "macos")]
                errors,
                #[cfg(target_os = "macos")]
                stream_id,
            ) {
                Ok(stream) => {
                    let _ = ready_tx.send(Ok(()));
                    // Returns on close signal or when the handle is gone.
                    let _ = close_rx.recv();
                    #[cfg(target_os = "macos")]
                    let _drop = macos_timing::Timing::start("close", "native_drop", stream_id);
                    drop(stream);
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                }
            }
        })
        .map_err(|e| OutputError::Backend(e.to_string()))?;
    let started = {
        #[cfg(target_os = "macos")]
        let _wait = macos_timing::Timing::start("open", "ready_wait", stream_id);
        ready_rx
            .recv()
            .unwrap_or_else(|_| Err(OutputError::Backend("output thread died".into())))
    };
    match started {
        Ok(()) => Ok(Box::new(CpalStream {
            identity,
            format,
            close: Some(close_tx),
            thread: Some(thread),
            #[cfg(target_os = "macos")]
            stream_id,
        })),
        Err(e) => {
            let _ = thread.join();
            Err(e)
        }
    }
}

/// Builds and starts a stream on `device`. Runs on the output thread.
fn build_stream(
    device: &Device,
    format: StreamFormat,
    mut renderer: Box<dyn Renderer>,
    events: OutputEventSink,
    #[cfg(target_os = "macos")] errors: Option<device_events::StreamErrors>,
    #[cfg(target_os = "macos")] stream_id: u64,
) -> Result<cpal::Stream, OutputError> {
    // `BufferSize::Default` on purpose: Fixed costs power on Android.
    let config = StreamConfig {
        channels: format.channels,
        sample_rate: format.sample_rate,
        buffer_size: BufferSize::Default,
    };
    let on_error = move |e: cpal::Error| {
        #[cfg(target_os = "macos")]
        if let Some(errors) = &errors {
            errors.record(e.kind());
        }
        #[cfg(target_os = "macos")]
        let event = event_for_macos(e.kind(), errors.is_some());
        #[cfg(not(target_os = "macos"))]
        let event = event_for(e.kind());
        if let Some(event) = event {
            events.report(event);
        }
    };

    // The data callbacks must not allocate, lock, log or do I/O.
    macro_rules! build {
        ($variant:ident) => {
            device.build_output_stream(
                config,
                move |data, cb_info| {
                    let ts = cb_info.timestamp();
                    let info = RenderInfo {
                        playback_delay: Some(ts.playback.duration_since(ts.callback)),
                    };
                    renderer.render(SampleBufMut::$variant(data), &info);
                },
                on_error,
                None,
            )
        };
    }
    #[cfg(target_os = "macos")]
    let building = macos_timing::Timing::start("open", "build_stream", stream_id);
    let stream = match format.sample_type {
        SampleType::F32 => build!(F32),
        SampleType::I16 => build!(I16),
        SampleType::I32 => build!(I32),
    }
    .map_err(backend_err)?;
    #[cfg(target_os = "macos")]
    drop(building);
    #[cfg(target_os = "macos")]
    let _play = macos_timing::Timing::start("open", "play", stream_id);
    stream.play().map_err(backend_err)?;
    Ok(stream)
}

/// Handle to the output thread; dropping it closes the stream.
struct CpalStream {
    identity: OutputIdentity,
    #[cfg(target_os = "macos")]
    stream_id: u64,
    format: StreamFormat,
    close: Option<Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl OutputStream for CpalStream {
    fn device_info(&self) -> Option<DeviceInfo> {
        self.identity.get()
    }
    fn format(&self) -> StreamFormat {
        self.format
    }
}

impl Drop for CpalStream {
    fn drop(&mut self) {
        #[cfg(target_os = "macos")]
        let _close = macos_timing::Timing::start("close", "total", self.stream_id);
        if let Some(tx) = self.close.take() {
            let _ = tx.send(());
        }
        if let Some(t) = self.thread.take() {
            #[cfg(target_os = "macos")]
            let _join = macos_timing::Timing::start("close", "join", self.stream_id);
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_types_map_directly() {
        assert_eq!(native_sample_type(SampleFormat::F32), Some(SampleType::F32));
        assert_eq!(native_sample_type(SampleFormat::I16), Some(SampleType::I16));
        assert_eq!(native_sample_type(SampleFormat::I32), Some(SampleType::I32));
        assert_eq!(native_sample_type(SampleFormat::U8), None);
        assert_eq!(native_sample_type(SampleFormat::F64), None);
    }

    #[test]
    fn fallback_prefers_f32_then_i32_then_i16() {
        use SampleFormat::*;
        assert_eq!(
            pick_sample_type(&[I16, I32, F32, U8]),
            Some(SampleType::F32)
        );
        assert_eq!(pick_sample_type(&[I16, I32]), Some(SampleType::I32));
        assert_eq!(pick_sample_type(&[U8, I16]), Some(SampleType::I16));
        assert_eq!(pick_sample_type(&[U8, F64]), None);
        assert_eq!(pick_sample_type(&[]), None);
    }

    #[test]
    fn error_kinds_map_to_events() {
        assert_eq!(
            event_for(ErrorKind::DeviceChanged),
            Some(OutputEvent::Rerouted)
        );
        assert_eq!(
            event_for(ErrorKind::DeviceNotAvailable),
            Some(OutputEvent::DeviceUnavailable)
        );
        assert_eq!(
            event_for(ErrorKind::StreamInvalidated),
            Some(OutputEvent::StreamInvalidated)
        );
        assert_eq!(event_for(ErrorKind::Xrun), Some(OutputEvent::Xrun));
        assert_eq!(
            event_for(ErrorKind::BackendError),
            Some(OutputEvent::Unknown)
        );
    }

    #[test]
    fn stream_handle_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<CpalStream>();
        assert_send::<CpalBackend>();
    }
    #[test]
    fn macos_monitor_owns_reroutes_but_other_errors_and_fallback_keep_their_mapping() {
        for kind in [
            ErrorKind::DeviceChanged,
            ErrorKind::DeviceNotAvailable,
            ErrorKind::StreamInvalidated,
            ErrorKind::Xrun,
            ErrorKind::BackendError,
        ] {
            assert_eq!(event_for_macos(kind, false), event_for(kind));
            if kind == ErrorKind::DeviceChanged {
                assert_eq!(event_for_macos(kind, true), None);
            } else {
                assert_eq!(event_for_macos(kind, true), event_for(kind));
            }
        }
    }
}
