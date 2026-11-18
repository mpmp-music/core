//! Default activation: existing caller, fresh COM owner, then concrete endpoint.
use super::*;
use crate::owner::Owner;
use crate::renderers::{Renderers, discard_failed_candidate};
use mpmp_engine::output::retry::{retry_on_endpoint, retry_on_fresh};
use std::collections::HashMap;
use std::sync::{Arc, Weak};

#[derive(Default)]
pub(crate) struct OwnerState {
    streams: HashMap<u64, cpal::Stream>,
    next: u64,
}

pub(crate) type AudioOwner = Owner<OwnerState>;

pub(crate) struct Prepared {
    device: Device,
    endpoint: String,
    automatic: bool,
    fresh: bool,
    owner: Option<Arc<AudioOwner>>,
}

// If a successful native open arrives after its caller timed out, returning this
// lease to a disconnected channel queues owner-side destruction instead of leaking
// an unclaimed stream. Accepted leases retain the same cleanup obligation.
struct CandidateToken {
    owner: Arc<AudioOwner>,
    token: Option<u64>,
}

impl Drop for CandidateToken {
    fn drop(&mut self) {
        if let Some(token) = self.token.take() {
            self.owner.post_cleanup(move |state| {
                state.streams.remove(&token);
            });
        }
    }
}

struct OwnedStream {
    identity: OutputIdentity,
    owner: Arc<AudioOwner>,
    token: u64,
    format: StreamFormat,
}

impl OutputStream for OwnedStream {
    fn device_info(&self) -> Option<DeviceInfo> {
        self.identity.get()
    }
    fn format(&self) -> StreamFormat {
        self.format
    }
}

impl Drop for OwnedStream {
    fn drop(&mut self) {
        let token = self.token;
        let _ = self.owner.cleanup_and_wait("close", move |state| {
            state.streams.remove(&token);
        });
    }
}

fn default_id(host: &cpal::Host) -> Result<cpal::DeviceId, OutputError> {
    host.default_output_device()
        .ok_or(OutputError::NoDevice)?
        .id()
        .map_err(backend_err)
}

fn query(
    host: &cpal::Host,
    concrete: bool,
    devices: &Weak<Mutex<HashMap<String, Device>>>,
) -> Result<(OutputCaps, Prepared), OutputError> {
    let id = default_id(host)?;
    let device = if concrete {
        // Background discovery supplies DeviceHandle::Specific for IMMDevice::Activate.
        devices
            .upgrade()
            .ok_or(OutputError::NoDevice)?
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&id.to_string())
            .cloned()
            .ok_or(OutputError::NoDevice)?
    } else {
        host.default_output_device().ok_or(OutputError::NoDevice)?
    };
    tracing::info!(stage =
    if concrete {
     "concrete-owner" }
     else {
     "default" }
    ,
     endpoint = %id.id(),
     operation = "capabilities",
     thread_id = ?thread::current().id(),
     "querying audio endpoint");

    let caps = device_capabilities(&device)?;
    if default_id(host)? != id {
        return Err(OutputError::Backend(
            "default changed during capabilities query".into(),
        ));
    }
    Ok((
        caps,
        Prepared {
            device,
            endpoint: id.id().to_owned(),
            automatic: !concrete,
            fresh: false,
            owner: None,
        },
    ))
}

fn owner_query(
    owner: &Arc<AudioOwner>,
    concrete: bool,
    devices: Weak<Mutex<HashMap<String, Device>>>,
) -> Result<(OutputCaps, Prepared), OutputError> {
    let result = owner.call("capabilities", move |_| {
        query(&cpal::default_host(), concrete, &devices)
    });
    tracing::info!(stage =
    if concrete {
     "concrete-owner" }
     else {
     "default-owner" }
    ,
     operation = "capabilities",
     success = result.is_ok(),
     error = ?result.as_ref().err(),
     "audio activation attempt");

    let (caps, mut prepared) = result?;
    prepared.owner = Some(owner.clone());
    prepared.fresh = true;
    Ok((caps, prepared))
}

pub(crate) fn capabilities_read(
    id: Option<DeviceId>,
    devices: Weak<Mutex<HashMap<String, Device>>>,
    preferred: Option<Arc<AudioOwner>>,
    cancel: mpmp_engine::output_task::Cancellation,
) -> Result<(OutputCaps, Prepared), OutputError> {
    if let Some(id) = id {
        let device = devices
            .upgrade()
            .ok_or(OutputError::NoDevice)?
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&id.0)
            .cloned()
            .ok_or(OutputError::NoDevice)?;
        let read = move || -> Result<(OutputCaps, Prepared), OutputError> {
            let endpoint = device.id().map_err(backend_err)?.id().to_owned();
            let caps = device_capabilities(&device)?;
            Ok((
                caps,
                Prepared {
                    device,
                    endpoint,
                    automatic: false,
                    fresh: false,
                    owner: None,
                },
            ))
        };
        if let Some(owner) = preferred {
            let (caps, mut prepared) = owner.call("capabilities", move |_| read())?;
            prepared.owner = Some(owner);
            return Ok((caps, prepared));
        }
        return read();
    }
    let primary = match preferred {
        Some(owner) => owner_query(&owner, false, devices.clone()).map(|(caps, mut prepared)| {
            prepared.fresh = false;
            (caps, prepared)
        }),
        None => query(&cpal::default_host(), false, &devices),
    };
    tracing::info!(stage = "preferred-default",
     operation = "capabilities",
     success = primary.is_ok(),
     error = ?primary.as_ref().err(),
     "audio activation attempt");

    let mut fresh = None;
    let result = retry_on_fresh(
        primary,
        || {
            cancel.check()?;
            let owner = AudioOwner::start(OwnerState::default)?;
            fresh = Some(owner.clone());
            owner_query(&owner, false, devices.clone())
        },
        "capabilities",
    );
    let (caps, prepared) = retry_on_endpoint(
        result,
        || {
            cancel.check()?;
            let owner = match fresh {
                Some(owner) => owner,
                None => AudioOwner::start(OwnerState::default)?,
            };
            let selected = owner_query(&owner, true, devices.clone())?;
            let endpoint = selected.1.endpoint.clone();
            Ok((selected, endpoint))
        },
        Ok,
        "capabilities",
    )?;
    Ok((caps, prepared))
}

impl CpalBackend {
    fn open_prepared(
        &mut self,
        prepared: Prepared,
        request: &StreamRequest,
        renderer: Box<dyn Renderer>,
    ) -> Result<Box<dyn OutputStream>, OutputError> {
        let follows_default = request.device.is_none();
        if follows_default && default_id(&self.host)?.id() != prepared.endpoint {
            return Err(OutputError::Backend(
                "default endpoint changed before output open".into(),
            ));
        }
        if self.monitor.is_none() {
            self.monitor = Some(device_events::DeviceMonitor::start()?);
        }
        let identity = OutputIdentity::new(device_info(&prepared.device, follows_default)?);
        if let Some(monitor) = &self.monitor {
            monitor.output(
                prepared.endpoint.clone(),
                follows_default,
                prepared.automatic,
                false,
                Some(identity.clone()),
            );
        }
        let format = request.format;
        let events = request.events.clone();
        let stage = if prepared.owner.is_none() {
            "default-existing"
        } else if prepared.automatic {
            "default-owner"
        } else {
            "concrete-owner"
        };
        let result: Result<Box<dyn OutputStream>, OutputError> =
            if let Some(owner) = &prepared.owner {
                let device = prepared.device;
                let candidate_owner = owner.clone();
                owner
                    .call_guarded("open", move |state, active| {
                        tracing::info!(thread_id = ?thread::current().id(),
                         operation = "open",
                         "opening audio on owner thread");

                        let stream = build_stream(&device, format, renderer, events)?;
                        active.check()?;
                        state.next += 1;
                        let token = state.next;
                        state.streams.insert(token, stream);
                        Ok(CandidateToken {
                            owner: candidate_owner,
                            token: Some(token),
                        })
                    })
                    .map(|mut candidate| {
                        let token = candidate.token.take().unwrap();
                        Box::new(OwnedStream {
                            identity,
                            owner: owner.clone(),
                            token,
                            format,
                        }) as Box<dyn OutputStream>
                    })
            } else {
                spawn_output(prepared.device, format, identity, renderer, events)
            };
        tracing::info!(stage,
         operation = "open",
         endpoint = %prepared.endpoint,
         success = result.is_ok(),
         error = ?result.as_ref().err(),
         "audio activation attempt");

        let stream = result?;
        if follows_default && default_id(&self.host)?.id() != prepared.endpoint {
            drop(stream);
            return Err(OutputError::Backend(
                "default endpoint changed during output open".into(),
            ));
        }
        if prepared.automatic
            && let Some(owner) = prepared.owner
        {
            // A complete fresh query+open worked. Keep its apartment for future setup.
            *self.owner.lock().unwrap_or_else(|e| e.into_inner()) = Some(owner);
        }
        Ok(stream)
    }

    fn open_queried(
        &mut self,
        owner: Arc<AudioOwner>,
        concrete: bool,
        request: &StreamRequest,
        renderers: &Renderers,
    ) -> Result<Box<dyn OutputStream>, OutputError> {
        let devices = Arc::downgrade(&self.devices);
        let (caps, prepared) = mpmp_engine::output_task::OutputTask::start(move |cancel| {
            cancel.check()?;
            owner_query(&owner, concrete, devices)
        })?
        .wait(mpmp_engine::output_task::READ_TIMEOUT)?;
        if caps.mix_format != request.format {
            return Err(OutputError::UnsupportedFormat(
                "default format changed during retry; requery required".into(),
            ));
        }
        self.open_prepared(prepared, request, Box::new(renderers.take()?))
    }

    pub(crate) fn windows_open(
        &mut self,
        request: StreamRequest,
        renderer: Box<dyn Renderer>,
    ) -> Result<Box<dyn OutputStream>, OutputError> {
        let prepared = self
            .prepared
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if request.device.is_some() {
            if let Some(prepared) = prepared {
                return self.open_prepared(prepared, &request, renderer);
            }
            let device = self.resolve(request.device.as_ref())?;
            let endpoint = device.id().map_err(backend_err)?.id().to_owned();
            return self.open_prepared(
                Prepared {
                    device,
                    endpoint,
                    automatic: false,
                    fresh: false,
                    owner: None,
                },
                &request,
                renderer,
            );
        }
        let selected = prepared.map_or_else(
            || {
                self.capabilities(None)?;
                self.prepared
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take()
                    .ok_or(OutputError::NoDevice)
            },
            Ok,
        );
        let already_concrete = selected.as_ref().is_ok_and(|p| !p.automatic);
        let already_fresh = selected.as_ref().is_ok_and(|p| p.fresh);
        let renderers = Renderers::new(renderer);
        let primary =
            selected.and_then(|p| self.open_prepared(p, &request, Box::new(renderers.take()?)));
        if primary.is_ok() || already_concrete {
            return primary;
        }
        discard_failed_candidate(&request.events);
        let mut fresh = None;
        let result = if already_fresh {
            // Query has already used a new apartment in this attempt.
            primary
        } else {
            retry_on_fresh(
                primary,
                || {
                    let owner = AudioOwner::start(OwnerState::default)?;
                    fresh = Some(owner.clone());
                    self.open_queried(owner, false, &request, &renderers)
                },
                "open",
            )
        };
        if result.is_ok() {
            return result;
        }
        discard_failed_candidate(&request.events);
        let devices = Arc::downgrade(&self.devices);
        retry_on_endpoint(
            result,
            || {
                let owner = match fresh {
                    Some(owner) => owner,
                    None => AudioOwner::start(OwnerState::default)?,
                };
                let (caps, prepared) =
                    mpmp_engine::output_task::OutputTask::start(move |cancel| {
                        cancel.check()?;
                        owner_query(&owner, true, devices)
                    })?
                    .wait(mpmp_engine::output_task::READ_TIMEOUT)?;
                if caps.mix_format != request.format {
                    return Err(OutputError::UnsupportedFormat(
                        "default format changed during endpoint retry".into(),
                    ));
                }
                let endpoint = prepared.endpoint.clone();
                Ok((prepared, endpoint))
            },
            |p| self.open_prepared(p, &request, Box::new(renderers.take()?)),
            "open",
        )
    }
}
