use std::collections::HashMap;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{Sender, bounded, select, tick};
use crossbeam_queue::SegQueue;
use mpmp_engine::output::{OutputError, OutputEventSink, OutputIdentity, RouteFacts};
use windows::Win32::Foundation::PROPERTYKEY;
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::StructuredStorage::{PropVariantClear, PropVariantToUInt32};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize, STGM_READ,
};
use windows::core::{PCWSTR, implement};

use super::route::Route;

enum Notice {
    Changed,
    Default {
        id: Option<String>,
        at: Instant,
    },
    Format(String),
    Watch(OutputEventSink),
    Output {
        endpoint: String,
        follows_default: bool,
        automatic: bool,
        reset_previous: bool,
        identity: Option<OutputIdentity>,
    },
    Lost {
        id: String,
        removed: bool,
        state: DEVICE_STATE,
        at: Instant,
    },
}

// Callback strings are borrowed only for the duration of the notification.
unsafe fn endpoint_id(id: PCWSTR) -> Option<String> {
    if id.is_null() {
        return None;
    }
    unsafe { id.to_string() }.ok().filter(|id| !id.is_empty())
}

unsafe fn default_notice(flow: EDataFlow, role: ERole, id: PCWSTR, at: Instant) -> Option<Notice> {
    (flow == eRender && role == eConsole).then(|| Notice::Default {
        id: unsafe { endpoint_id(id) },
        at,
    })
}

unsafe fn property_notice(id: PCWSTR, key: &PROPERTYKEY) -> Option<Notice> {
    if key.fmtid != PKEY_AudioEngine_DeviceFormat.fmtid
        || key.pid != PKEY_AudioEngine_DeviceFormat.pid
    {
        return None;
    }
    unsafe { endpoint_id(id) }.map(Notice::Format)
}

#[implement(IMMNotificationClient)]
struct Notifications {
    pending: Arc<SegQueue<Notice>>,
}

#[allow(non_snake_case)]
impl IMMNotificationClient_Impl for Notifications_Impl {
    fn OnDeviceStateChanged(&self, id: &PCWSTR, state: DEVICE_STATE) -> windows::core::Result<()> {
        self.pending.push(Notice::Lost {
            id: unsafe { endpoint_id(*id) }.unwrap_or_default(),
            removed: false,
            state,
            at: Instant::now(),
        });
        Ok(())
    }
    fn OnDeviceAdded(&self, _: &PCWSTR) -> windows::core::Result<()> {
        self.pending.push(Notice::Changed);
        Ok(())
    }
    fn OnDeviceRemoved(&self, id: &PCWSTR) -> windows::core::Result<()> {
        self.pending.push(Notice::Lost {
            id: unsafe { endpoint_id(*id) }.unwrap_or_default(),
            removed: true,
            state: DEVICE_STATE_NOTPRESENT,
            at: Instant::now(),
        });
        Ok(())
    }
    fn OnDefaultDeviceChanged(
        &self,
        flow: EDataFlow,
        role: ERole,
        id: &PCWSTR,
    ) -> windows::core::Result<()> {
        if let Some(notice) = unsafe { default_notice(flow, role, *id, Instant::now()) } {
            self.pending.push(notice);
        }
        Ok(())
    }
    fn OnPropertyValueChanged(&self, id: &PCWSTR, key: &PROPERTYKEY) -> windows::core::Result<()> {
        if let Some(notice) = unsafe { property_notice(*id, key) } {
            self.pending.push(notice);
        }
        Ok(())
    }
}

fn endpoint(enumerator: &IMMDeviceEnumerator) -> Option<(String, bool)> {
    unsafe {
        let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole).ok()?;
        let raw = device.GetId().ok()?;
        let id = endpoint_id(PCWSTR(raw.0));
        windows::Win32::System::Com::CoTaskMemFree(Some(raw.0.cast()));
        let headphones = headphones(&device).unwrap_or(false);
        id.map(|id| (id, headphones))
    }
}

fn headphones(device: &IMMDevice) -> Option<bool> {
    unsafe {
        device
            .OpenPropertyStore(STGM_READ)
            .ok()
            .and_then(|s| s.GetValue(&PKEY_AudioEndpoint_FormFactor).ok())
            .and_then(|mut value| {
                let form = PropVariantToUInt32(&value).ok();
                let _ = PropVariantClear(&mut value);
                form.map(|form| form == Headphones.0 as u32 || form == Headset.0 as u32)
            })
    }
}

fn device(enumerator: &IMMDeviceEnumerator, id: &str) -> Option<IMMDevice> {
    let wide: Vec<u16> = id.encode_utf16().chain(Some(0)).collect();
    unsafe { enumerator.GetDevice(PCWSTR(wide.as_ptr())).ok() }
}

fn available(enumerator: &IMMDeviceEnumerator, id: &str) -> Option<bool> {
    unsafe {
        device(enumerator, id)?
            .GetState()
            .ok()
            .map(|s| s == DEVICE_STATE_ACTIVE)
    }
}

pub(crate) struct DeviceMonitor {
    pending: Arc<SegQueue<Notice>>,
    changed: Sender<()>,
    stop: Sender<()>,
    thread: Option<JoinHandle<()>>,
}

impl DeviceMonitor {
    pub fn start() -> Result<Self, OutputError> {
        let pending = Arc::new(SegQueue::new());
        let notices = Arc::clone(&pending);
        let (changed, changes) = bounded(1);
        let (stop, stopping) = bounded(1);
        let (ready, started) = bounded(1);
        let thread = std::thread::Builder::new()
            .name("audio-devices".into())
            .spawn(move || {
                let result = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok() };
                if let Err(error) = result {
                    let _ = ready.send(Err(error.to_string()));
                    return;
                }
                let result = (|| -> windows::core::Result<()> {
                    let enumerator: IMMDeviceEnumerator =
                        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };
                    let mut types = HashMap::new();
                    let mut route = Route {
                        follows_default: true,
                        ..Route::default()
                    };
                    if let Some((endpoint, headphones)) = endpoint(&enumerator) {
                        types.insert(endpoint.clone(), headphones);
                        route.endpoint = endpoint;
                        route.headphones = headphones;
                    }
                    let client: IMMNotificationClient = Notifications {
                        pending: Arc::clone(&notices),
                    }
                    .into();
                    unsafe {
                        enumerator.RegisterEndpointNotificationCallback(&client)?;
                    }
                    let _ = ready.send(Ok(()));
                    // Cache only observed endpoints. A full classification scan here
                    // could delay Watch/configuration and queued disconnect facts.
                    let polling = tick(Duration::from_millis(25));
                    loop {
                        select! {
                            recv(stopping) -> _ => break,
                            recv(changes) -> _ => {},
                            recv(polling) -> _ => { if notices.is_empty() { continue; } },
                        }
                        // Configuration and callbacks share FIFO order: a newer stream
                        // cannot replace the identity of an earlier queued loss.
                        for _ in 0..notices.len() {
                            let Some(notice) = notices.pop() else {
                                break;
                            };
                            if matches!(
                                &notice,
                                Notice::Changed | Notice::Default { .. } | Notice::Lost { .. }
                            ) && let Some(sink) = &route.sink
                            {
                                sink.report_route(RouteFacts {
                                    devices_changed: true,
                                    ..Default::default()
                                });
                            }
                            let route_notice =
                                matches!(&notice, Notice::Default { .. } | Notice::Changed);
                            match notice {
                                Notice::Format(id) => route.format_changed(&id),
                                Notice::Changed => {
                                    let new = endpoint(&enumerator);
                                    if let Some((id, headphones)) = &new {
                                        types.insert(id.clone(), *headphones);
                                    }
                                    let old_available = available(&enumerator, &route.endpoint);
                                    if !route.follows_default {
                                        route.fixed_available(old_available);
                                    } else {
                                        route.default_changed(
                                            new.as_ref().map(|(id, _)| id.clone()),
                                            new.is_some_and(|(_, h)| h),
                                            old_available,
                                            Instant::now(),
                                        );
                                    }
                                }
                                Notice::Default { id, at } => {
                                    let headphones = id.as_ref().is_some_and(|id| {
                                        if let Some(h) =
                                            device(&enumerator, id).and_then(|d| headphones(&d))
                                        {
                                            types.insert(id.clone(), h);
                                        }
                                        types.get(id).copied().unwrap_or(false)
                                    });
                                    let old_available = available(&enumerator, &route.endpoint);
                                    route.default_changed(id, headphones, old_available, at);
                                }
                                Notice::Watch(sink) => route.sink = Some(sink),
                                Notice::Lost {
                                    id,
                                    removed,
                                    state,
                                    at,
                                } => route.lost(
                                    &id,
                                    removed,
                                    removed || state != DEVICE_STATE_ACTIVE,
                                    state == DEVICE_STATE_UNPLUGGED
                                        || state == DEVICE_STATE_NOTPRESENT,
                                    at,
                                ),
                                Notice::Output {
                                    endpoint,
                                    follows_default,
                                    automatic,
                                    reset_previous,
                                    identity,
                                } => {
                                    route
                                        .identity
                                        .attach(identity, follows_default && automatic);
                                    let headphones = device(&enumerator, &endpoint)
                                        .and_then(|d| headphones(&d))
                                        .or_else(|| types.get(&endpoint).copied())
                                        .unwrap_or(false);
                                    types.insert(endpoint.clone(), headphones);
                                    route.configure(
                                        endpoint,
                                        headphones,
                                        follows_default,
                                        automatic,
                                        Instant::now(),
                                    );
                                    if reset_previous {
                                        route.selection_changed();
                                    }
                                }
                            }
                            // Identity resolution is worker-only and follows publication
                            // of safety facts. Never replace a pinned fallback's identity.
                            if route_notice
                                && route.identity.is_automatic()
                                && let Some(sink) = &route.sink
                            {
                                let id = cpal::DeviceId::new(cpal::HostId::Wasapi, &route.endpoint);
                                let observed = crate::confirmed_default_identity(&id);
                                route.identity.confirm(observed, sink);
                            }
                        }
                    }
                    unsafe {
                        enumerator.UnregisterEndpointNotificationCallback(&client)?;
                    }
                    Ok(())
                })();
                if let Err(error) = result {
                    let _ = ready.try_send(Err(error.to_string()));
                    tracing::warn!(%error, "device notifications stopped");
                }
                unsafe {
                    CoUninitialize();
                }
            })
            .map_err(|e| OutputError::Backend(e.to_string()))?;
        started
            .recv()
            .map_err(|e| OutputError::Backend(e.to_string()))?
            .map_err(OutputError::Backend)?;
        Ok(Self {
            pending,
            changed,
            stop,
            thread: Some(thread),
        })
    }

    pub fn is_running(&self) -> bool {
        self.thread.as_ref().is_some_and(|t| !t.is_finished())
    }

    pub fn watch(&self, sink: OutputEventSink) {
        self.pending.push(Notice::Watch(sink));
        let _ = self.changed.try_send(());
    }

    pub fn output(
        &self,
        endpoint: String,
        follows_default: bool,
        automatic: bool,
        reset_previous: bool,
        identity: Option<OutputIdentity>,
    ) {
        self.pending.push(Notice::Output {
            endpoint,
            follows_default,
            automatic,
            reset_previous,
            identity,
        });
        let _ = self.changed.try_send(());
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn property_callback_only_copies_device_format_notifications() {
        let id: Vec<_> = "endpoint-耳机".encode_utf16().chain(Some(0)).collect();
        let Some(Notice::Format(copied)) =
            (unsafe { property_notice(PCWSTR(id.as_ptr()), &PKEY_AudioEngine_DeviceFormat) })
        else {
            panic!("format notice missing")
        };
        assert_eq!(copied, "endpoint-耳机");
        assert!(
            unsafe { property_notice(PCWSTR::null(), &PKEY_AudioEngine_DeviceFormat) }.is_none()
        );
        assert!(
            unsafe { property_notice(PCWSTR(id.as_ptr()), &PKEY_AudioEndpoint_FormFactor) }
                .is_none()
        );
        let wrong_pid = PROPERTYKEY {
            pid: PKEY_AudioEngine_DeviceFormat.pid + 1,
            ..PKEY_AudioEngine_DeviceFormat
        };
        assert!(unsafe { property_notice(PCWSTR(id.as_ptr()), &wrong_pid) }.is_none());
    }

    #[test]
    fn default_callback_copies_null_empty_and_valid_ids() {
        let empty = [0_u16];
        let valid: Vec<_> = "endpoint-耳机".encode_utf16().chain(Some(0)).collect();
        let at = Instant::now();
        for (raw, expected) in [
            (PCWSTR::null(), None),
            (PCWSTR(empty.as_ptr()), None),
            (PCWSTR(valid.as_ptr()), Some("endpoint-耳机")),
        ] {
            let Some(Notice::Default { id, at: captured }) =
                (unsafe { default_notice(eRender, eConsole, raw, at) })
            else {
                panic!("render/console callback must produce a default notice");
            };
            assert_eq!(id.as_deref(), expected);
            assert_eq!(captured, at);
            assert!(unsafe { default_notice(eCapture, eConsole, raw, at) }.is_none());
            assert!(unsafe { default_notice(eRender, eMultimedia, raw, at) }.is_none());
        }
    }
}
