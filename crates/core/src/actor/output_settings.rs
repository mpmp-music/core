//! Output preferences and device discovery. Stream reconstruction stays in the actor's
//! existing freeze/accounting protocol; a preference receipt never promises audible audio.
use super::*;
use crate::contract::{
    CapabilitySupport, MAX_OUTPUT_DEVICES, MAX_SAFE_INTEGER, OutputActualInfo, OutputBackend,
    OutputDeviceCapabilities, OutputDeviceIdStability, OutputDeviceInfo, OutputDeviceSelection,
    OutputFailureInfo, OutputFallbackInfo, OutputFallbackPolicy, OutputFallbackReason,
    OutputRatePolicy, OutputSettings, OutputShareMode, OutputStateInfo, OutputStatus, Query,
    QueryResult, SampleIntegrity,
};

pub(super) fn valid_selection(selection: &OutputDeviceSelection) -> bool {
    match selection {
        OutputDeviceSelection::SystemDefault => true,
        OutputDeviceSelection::Device { id } => {
            !id.is_empty() && id.len() <= 4096 && !id.contains('\0')
        }
        _ => false,
    }
}

pub(super) fn valid_settings(settings: &OutputSettings) -> bool {
    valid_selection(&settings.selection)
        && settings.settings_revision <= MAX_SAFE_INTEGER
        && settings.mode == OutputShareMode::Shared
        && settings.rate_policy == OutputRatePolicy::Mix
        && matches!(
            settings.fallback,
            OutputFallbackPolicy::Pause | OutputFallbackPolicy::Shared
        )
}

pub(super) fn signal_format(format: mpmp_engine::output::StreamFormat) -> SignalFormat {
    SignalFormat {
        sample_rate: format.sample_rate,
        channels: format.channels,
        bits_per_sample: Some(format.sample_type.bits()),
        valid_bits_per_sample: (format.sample_type != mpmp_engine::output::SampleType::F32)
            .then_some(format.sample_type.bits()),
        sample_format: Some(match format.sample_type {
            mpmp_engine::output::SampleType::F32 => SampleFormat::F32,
            mpmp_engine::output::SampleType::I16 => SampleFormat::I16,
            mpmp_engine::output::SampleType::I32 => SampleFormat::I32,
        }),
    }
}

pub(super) struct PendingCapabilities {
    task: mpmp_engine::output_task::OutputTask<mpmp_engine::output::OutputCaps>,
    reply: Sender<QueryResult>,
    device: OutputDeviceInfo,
    revision: u64,
    epoch: u64,
    generation: u64,
    deadline: Instant,
}

fn discovery_error(error: mpmp_engine::output::OutputError) -> ContractError {
    output_read_error(&error.to_string())
}

fn output_read_error(message: &str) -> ContractError {
    ContractError::failed(ErrorCode::OutputDeviceUnavailable, message).retryable()
}

fn capability_result(
    generation: u64,
    resolved_device: OutputDeviceInfo,
    caps: mpmp_engine::output::OutputCaps,
) -> QueryResult {
    QueryResult::OutputDeviceCapabilities {
        generation,
        resolved_device,
        capabilities: OutputDeviceCapabilities {
            backend: OutputBackend::Cpal,
            exclusive_access: CapabilitySupport::Unsupported,
            source_rate_switch: CapabilitySupport::Unsupported,
            bit_exact_transport: CapabilitySupport::Undetermined,
            physical_route_known: false,
            mix_format: Some(signal_format(caps.mix_format)),
            exclusive_formats: Vec::new(),
            formats_complete: true,
        },
    }
}

impl Actor {
    /// Invalidates the current discovery observation. While a task is busy only
    /// one trailing refresh is retained; neither actor nor queries enumerate.
    pub(super) fn refresh_output_devices(&mut self) -> Result<(), ContractError> {
        self.output.output_discovery_epoch += 1;
        self.output.output_discovery_retry = None;
        self.output.output_discovery_pending = true;
        self.start_output_discovery();
        Ok(())
    }

    fn start_output_discovery(&mut self) {
        if self.output.output_discovery_busy.is_some() || !self.output.output_discovery_pending {
            return;
        }
        let Some(discover) = self.engine.device_discovery() else {
            return;
        };
        let revision = self.output.output_settings.settings_revision;
        let epoch = self.output.output_discovery_epoch;
        self.output.output_discovery_pending = false;
        self.output.output_discovery_busy = Some((revision, epoch));
        match mpmp_engine::output_task::OutputTask::start(move |cancel| discover(cancel)) {
            Ok(task) => self.output.output_discovery_task = Some(task),
            Err(error) => self.output_discovered(revision, epoch, Err(error)),
        }
    }

    pub(super) fn output_discovered(
        &mut self,
        revision: u64,
        epoch: u64,
        result: Result<mpmp_engine::output::DiscoveredDevices, mpmp_engine::output::OutputError>,
    ) {
        if self.output.output_discovery_busy != Some((revision, epoch)) {
            return;
        }
        self.output.output_discovery_task.take();
        self.output.output_discovery_busy = None;
        if revision != self.output.output_settings.settings_revision
            || epoch != self.output.output_discovery_epoch
        {
            self.output.output_discovery_pending = true;
        } else {
            match result.map_err(discovery_error).and_then(|result| {
                // Validate the complete public list before committing either view.
                let mut count = result.devices.len();
                if let OutputDeviceSelection::Device { id } = &self.output.output_settings.selection
                    && self.output.output_selected.is_some()
                    && !result.devices.iter().any(|d| d.id.0 == *id)
                {
                    count += 1;
                }
                if count > MAX_OUTPUT_DEVICES {
                    return Err(ContractError::invalid(
                        ErrorCode::OutOfRange,
                        "output device list exceeds 256 entries",
                    ));
                }
                let devices = result.devices.clone();
                self.engine.commit_output_discovery(result);
                self.accept_output_devices(devices)
            }) {
                Ok(()) => {
                    self.output.output_devices_error = None;
                    self.output.output_discovery_retry = None;
                    self.apply_discovered_output();
                }
                Err(error) => {
                    self.output.output_devices_error = Some(error);
                    let attempt = self
                        .output
                        .output_discovery_retry
                        .map_or(0, |(_, attempt)| attempt + 1);
                    if attempt < 3 {
                        self.output.output_discovery_retry =
                            Some((self.now_ms() + [300, 1000, 3000][attempt], attempt));
                    } else {
                        self.output.output_discovery_retry = Some((u64::MAX, attempt));
                        // Exhaustion ends the fast retry round, not discovery.
                        self.output.output_devices_poll_at = self.now_ms() + 30_000;
                    }
                }
            }
        }
        self.start_output_discovery();
    }

    pub(super) fn poll_output_reads(&mut self) {
        if let Some(result) = self
            .output
            .output_discovery_task
            .as_ref()
            .and_then(|task| task.try_take())
        {
            let (revision, epoch) = self.output.output_discovery_busy.unwrap();
            self.output_discovered(revision, epoch, result);
        }
        if self.output.output_discovery_busy.is_none()
            && self
                .output
                .output_discovery_retry
                .is_some_and(|(at, _)| at <= self.now_ms())
        {
            // Retries retain their budget even with native notifications enabled.
            self.output.output_discovery_pending = true;
            self.start_output_discovery();
        }
        let now = Instant::now();
        let mut index = 0;
        while index < self.output.output_capability_tasks.len() {
            let pending = &self.output.output_capability_tasks[index];
            let result = pending.task.try_take();
            let stale = pending.revision != self.output.output_settings.settings_revision
                || pending.epoch != self.output.output_discovery_epoch
                || pending.generation != self.output.output_devices_generation;
            if result.is_some() || stale || now >= pending.deadline {
                let pending = self.output.output_capability_tasks.swap_remove(index);
                let result = if stale {
                    Err(output_read_error(
                        "output observation changed during capability read",
                    ))
                } else if now >= pending.deadline {
                    Err(output_read_error("output capability read timed out"))
                } else {
                    result
                        .unwrap()
                        .map_err(discovery_error)
                        .map(|caps| capability_result(pending.generation, pending.device, caps))
                };
                let _ = pending
                    .reply
                    .send(result.unwrap_or_else(|error| QueryResult::Err { error }));
            } else {
                index += 1;
            }
        }
    }

    pub(super) fn cancel_output_reads(&mut self) {
        let mut controls = Vec::new();
        if let Some(task) = self.output.output_discovery_task.take() {
            controls.push(task.cancellation());
            drop(task);
        }
        for pending in self.output.output_capability_tasks.drain(..) {
            controls.push(pending.task.cancellation());
            drop(pending);
        }
        self.output.output_discovery_pending = false;
        self.output.output_discovery_busy = None;
        self.output.output_discovery_retry = None;
        let deadline = Instant::now() + mpmp_engine::output_task::SHUTDOWN_TIMEOUT;
        for control in controls {
            control.wait_until(deadline);
        }
    }

    pub(super) fn handle_output_query(&mut self, query: Query, reply: Sender<QueryResult>) {
        if let Query::OutputDeviceCapabilities { selection } = query {
            let result = self
                .resolve_capability_device(&selection)
                .and_then(|device| {
                    if self.output.output_capability_tasks.len() >= 16 {
                        return Err(output_read_error(
                            "too many pending output capability reads",
                        ));
                    }
                    let id = mpmp_engine::output::DeviceId(device.id.clone());
                    let read = self
                        .engine
                        .output_capability_query(Some(&id))
                        .map_err(|e| output_read_error(&e.to_string()))?;
                    let task = mpmp_engine::output_task::OutputTask::start(read)
                        .map_err(discovery_error)?;
                    self.output
                        .output_capability_tasks
                        .push(PendingCapabilities {
                            task,
                            reply: reply.clone(),
                            device,
                            revision: self.output.output_settings.settings_revision,
                            epoch: self.output.output_discovery_epoch,
                            generation: self.output.output_devices_generation,
                            deadline: Instant::now() + mpmp_engine::output_task::READ_TIMEOUT,
                        });
                    Ok(())
                });
            if let Err(error) = result {
                let _ = reply.send(QueryResult::Err { error });
            }
        } else {
            let result = self.output_query(query);
            let _ = reply.send(result);
        }
    }

    fn resolve_capability_device(
        &self,
        selection: &OutputDeviceSelection,
    ) -> Result<OutputDeviceInfo, ContractError> {
        if *selection == OutputDeviceSelection::Unknown {
            return Err(ContractError::unknown_variant("output selection").with_field("selection"));
        }
        if !valid_selection(selection) {
            return Err(ContractError::invalid(
                ErrorCode::InvalidQuery,
                "invalid output selection",
            ));
        }
        if let Some(error) = &self.output.output_devices_error {
            return Err(error.clone());
        }
        self.output
            .output_devices
            .iter()
            .find(|d| {
                d.available
                    && match selection {
                        OutputDeviceSelection::SystemDefault => d.is_default,
                        OutputDeviceSelection::Device { id } => d.id == *id,
                        _ => false,
                    }
            })
            .cloned()
            .ok_or_else(|| {
                ContractError::not_found(
                    ErrorCode::OutputDeviceNotFound,
                    "output device is not available",
                )
            })
    }

    fn accept_output_devices(
        &mut self,
        devices: Vec<mpmp_engine::output::DeviceInfo>,
    ) -> Result<(), ContractError> {
        let mut devices: Vec<_> = devices
            .into_iter()
            .map(|d| OutputDeviceInfo {
                id: d.id.0,
                name: d.name,
                is_default: d.is_default,
                available: true,
                id_stability: if d.persistent {
                    OutputDeviceIdStability::Persistent
                } else {
                    OutputDeviceIdStability::Session
                },
            })
            .collect();
        if let OutputDeviceSelection::Device { id } = &self.output.output_settings.selection {
            if let Some(device) = devices.iter().find(|d| d.id == *id) {
                self.output.output_selected = Some(device.clone());
            } else if let Some(mut device) = self.output.output_selected.clone() {
                device.available = false;
                device.is_default = false;
                devices.push(device);
            }
        }
        if devices.len() > MAX_OUTPUT_DEVICES {
            return Err(ContractError::invalid(
                ErrorCode::OutOfRange,
                "output device list exceeds 256 entries",
            ));
        }
        if let OutputDeviceSelection::Device { id } = &self.output.output_settings.selection {
            let before = self
                .output
                .output_devices
                .iter()
                .any(|d| d.id == *id && d.available);
            let after = devices.iter().any(|d| d.id == *id && d.available);
            self.output.output_discovery_lost |= before && !after;
            self.output.output_discovery_arrived |= !before && after;
        }
        // Enumeration order is not a device change. Names never participate in identity.
        devices.sort_by(|a, b| a.id.cmp(&b.id));
        if self.output.output_devices != devices {
            self.output.output_devices = devices;
            self.output.output_devices_generation += 1;
            self.emit(Event::OutputDevicesChanged {
                generation: self.output.output_devices_generation,
            });
        }
        Ok(())
    }

    pub(super) fn poll_output_devices(&mut self) {
        self.poll_output_reads();
        if self.engine.has_native_output_notifications() {
            return;
        }
        let now = self.now_ms();
        if now < self.output.output_devices_poll_at {
            return;
        }
        let exhausted = self
            .output
            .output_discovery_retry
            .is_some_and(|(at, _)| at == u64::MAX);
        self.output.output_devices_poll_at = now
            + if self.playback.state == PlayState::Playing && !exhausted {
                1000
            } else {
                30_000
            };
        if self.output.output_discovery_busy.is_none()
            && (self.output.output_discovery_retry.is_none() || exhausted)
        {
            let _ = self.refresh_output_devices();
        }
    }

    fn apply_discovered_output(&mut self) {
        // A discovery result is reduced once, independently of cache reads.
        // Arrival never cancels a loss found in the same observation.
        let lost = std::mem::take(&mut self.output.output_discovery_lost);
        let arrived = std::mem::take(&mut self.output.output_discovery_arrived);
        if matches!(
            self.output.output_settings.selection,
            OutputDeviceSelection::Device { .. }
        ) {
            if lost
                && self.playback.playing.is_some()
                && (self.engine.is_active() || self.output.output_error.is_none())
            {
                self.output_action(policy::Action::PauseClose);
                self.output_failed(&EngineError::Output(
                    mpmp_engine::output::OutputError::NoDevice,
                ));
                self.output_unavailable();
            }
            if arrived {
                if self.playback.playing.is_some() && self.output.output_at.is_some() {
                    self.output.output_policy.external_change();
                    self.output_action(policy::Action::PauseRebuild);
                } else if self.output.output_selection_failure.is_some()
                    && self.engine.default_output_format().is_ok()
                {
                    self.output.output_selection_failure = None;
                }
            }
        }
        // Inventory refreshes announce output changes without spurious transport
        // StateChanged events. Pause/rebuild above emits its own state transition.
        self.emit_output_as(self.publication.cause.clone());
    }

    pub(super) fn output_query(&mut self, query: Query) -> QueryResult {
        let result = (|| {
            if let Query::OutputDeviceCapabilities {
                selection: OutputDeviceSelection::Unknown,
            } = query
            {
                return Err(
                    ContractError::unknown_variant("output selection").with_field("selection")
                );
            }
            if !matches!(query, Query::OutputState)
                && let Some(error) = &self.output.output_devices_error
            {
                return Err(error.clone());
            }
            Ok(match query {
                Query::OutputDevices => QueryResult::OutputDevices {
                    generation: self.output.output_devices_generation,
                    default_device: self
                        .output
                        .output_devices
                        .iter()
                        .find(|d| d.is_default)
                        .map(|d| d.id.clone()),
                    devices: self.output.output_devices.clone(),
                },
                Query::OutputDeviceCapabilities { .. } => {
                    return Err(output_read_error(
                        "capability reads require asynchronous dispatch",
                    ));
                }
                Query::OutputState => QueryResult::OutputState {
                    settings: self.output.output_settings.clone(),
                    state: self.output_state(),
                },
                _ => unreachable!(),
            })
        })();
        self.publish();
        result.unwrap_or_else(|error| QueryResult::Err { error })
    }

    pub(super) fn output_state(&self) -> OutputStateInfo {
        let actual = self
            .engine
            .output_format()
            .zip(self.engine.output_device())
            .map(|(format, device)| OutputActualInfo {
                device_id: device.id.0,
                name: device.name,
                share_mode: OutputShareMode::Shared,
                backend: OutputBackend::Cpal,
                format: signal_format(format),
                physical_device_id: None,
                exclusive_verified: false,
                sample_integrity: SampleIntegrity::Unverified,
            });
        let failure = self
            .output
            .output_error
            .as_ref()
            .map(|error| OutputFailureInfo {
                code: ErrorCode::OutputDeviceUnavailable,
                message: error.message.clone(),
                retryable: true,
            })
            .or_else(|| self.output.output_selection_failure.clone());
        OutputStateInfo {
            settings_revision: self.output.output_settings.settings_revision,
            generation: self.output.output_devices_generation,
            status: if failure.is_some() {
                OutputStatus::Unavailable
            } else if self.output.output_at.is_some() {
                OutputStatus::Rebuilding
            } else if self.engine.is_active() {
                OutputStatus::Ready
            } else {
                OutputStatus::Idle
            },
            actual,
            failure,
            fallback: self.output.output_fallback.clone(),
        }
    }

    fn output_revision(&self, expected: Option<u64>) -> Result<(), ContractError> {
        if expected.is_some_and(|r| r > MAX_SAFE_INTEGER) {
            return Err(ContractError::invalid(
                ErrorCode::OutOfRange,
                "output revision exceeds the safe integer range",
            )
            .with_field("command.expected_revision"));
        }
        if expected.is_some_and(|r| r != self.output.output_settings.settings_revision) {
            return Err(ContractError::conflict(
                ErrorCode::OutputSettingsConflict,
                "output settings changed",
                expected.unwrap(),
                self.output.output_settings.settings_revision,
            ));
        }
        if self.output.output_settings.settings_revision >= MAX_SAFE_INTEGER {
            return Err(ContractError::invalid(
                ErrorCode::OutOfRange,
                "output settings revision exhausted",
            ));
        }
        Ok(())
    }

    pub(super) fn set_output_mode(
        &mut self,
        mode: Option<OutputShareMode>,
        fallback: Option<OutputFallbackPolicy>,
        rate_policy: Option<OutputRatePolicy>,
        expected: Option<u64>,
    ) -> Result<bool, ContractError> {
        for (unknown, field) in [
            (mode == Some(OutputShareMode::Unknown), "command.mode"),
            (
                fallback == Some(OutputFallbackPolicy::Unknown),
                "command.fallback",
            ),
            (
                rate_policy == Some(OutputRatePolicy::Unknown),
                "command.rate_policy",
            ),
        ] {
            if unknown {
                return Err(ContractError::unknown_variant("output preference").with_field(field));
            }
        }
        self.output_revision(expected)?;
        let mut next = self.output.output_settings.clone();
        if let Some(mode) = mode {
            next.mode = mode;
        }
        if let Some(fallback) = fallback {
            next.fallback = fallback;
        }
        if let Some(rate) = rate_policy {
            next.rate_policy = rate;
        }
        if !valid_settings(&next) {
            return Err(ContractError::unsupported(
                ErrorCode::OutputModeUnsupported,
                "this build supports only shared + mix; fallback is stored but inactive on cpal",
            ));
        }
        if next == self.output.output_settings {
            return Ok(false);
        }
        next.settings_revision += 1;
        self.output.output_settings = next;
        self.output.output_fallback = None;
        self.persistence.dirty = true;
        self.emit_state();
        Ok(true)
    }

    pub(super) fn set_output_device(
        &mut self,
        selection: OutputDeviceSelection,
        expected: Option<u64>,
    ) -> Result<bool, ContractError> {
        if selection == OutputDeviceSelection::Unknown {
            return Err(
                ContractError::unknown_variant("output selection").with_field("command.selection")
            );
        }
        if !valid_selection(&selection) {
            return Err(ContractError::invalid(
                ErrorCode::InvalidRequest,
                "invalid output device identity",
            ));
        }
        self.output_revision(expected)?;
        if self.output.output_settings.selection == selection {
            if self
                .output
                .output_discovery_retry
                .is_some_and(|(at, _)| at == u64::MAX)
            {
                // An explicit retry refreshes discovery without changing settings
                // or rebuilding an otherwise healthy stream.
                let _ = self.refresh_output_devices();
            }
            return Ok(false);
        }
        // Settle exactly once before changing the target. Rebuild uses the same
        // playback instance and final rendered frame, preserving heard accounting.
        self.close_output();
        self.output.output_policy.cancel();
        self.output.output_selected = match &selection {
            OutputDeviceSelection::Device { id } => Some(
                self.output
                    .output_devices
                    .iter()
                    .find(|d| d.id == *id)
                    .cloned()
                    .unwrap_or_else(|| OutputDeviceInfo {
                        id: id.clone(),
                        name: id.clone(),
                        available: false,
                        is_default: false,
                        id_stability: if self
                            .engine
                            .output_identity_persistent(&mpmp_engine::output::DeviceId(id.clone()))
                        {
                            OutputDeviceIdStability::Persistent
                        } else {
                            OutputDeviceIdStability::Session
                        },
                    }),
            ),
            _ => None,
        };
        self.output.output_settings.selection = selection;
        self.output.output_settings.settings_revision += 1;
        self.output.output_fallback = None;
        self.output.output_selection_failure = None;
        self.output_recovered();
        self.configure_output_device();
        let _ = self.refresh_output_devices();
        self.output.output_discovery_lost = false;
        self.output.output_discovery_arrived = false;
        self.persistence.dirty = true;
        self.emit_state();
        if self.playback.playing.is_some() {
            self.output.output_policy.prepare_output();
            if self.rebuild_output() == RebuildResult::Failed {
                self.pause();
                self.output_unavailable();
            }
        } else if let Err(error) = self.engine.default_output_format() {
            self.output.output_selection_failure = Some(OutputFailureInfo {
                code: ErrorCode::OutputDeviceUnavailable,
                message: error.to_string(),
                retryable: true,
            });
        }
        self.emit_state();
        self.emit_position();
        Ok(true)
    }

    pub(super) fn configure_output_device(&mut self) {
        self.engine
            .set_output_device(match &self.output.output_settings.selection {
                OutputDeviceSelection::Device { id } => {
                    Some(mpmp_engine::output::DeviceId(id.clone()))
                }
                _ => None,
            });
    }

    pub(super) fn restore_output(&mut self, saved: Option<crate::state_file::SavedOutput>) {
        if let Some(saved) = saved.filter(|s| valid_settings(&s.settings)) {
            self.output.output_settings = saved.settings;
            self.output.output_selected = saved.device;
            if matches!(
                self.output.output_settings.selection,
                OutputDeviceSelection::Device { .. }
            ) && self
                .output
                .output_selected
                .as_ref()
                .is_none_or(|d| d.id_stability != OutputDeviceIdStability::Persistent)
            {
                self.output.output_settings.selection = OutputDeviceSelection::SystemDefault;
                self.output.output_selected = None;
                self.output.output_settings.settings_revision = self
                    .output
                    .output_settings
                    .settings_revision
                    .saturating_add(1)
                    .min(MAX_SAFE_INTEGER);
                self.output.output_fallback = Some(OutputFallbackInfo {
                    reason: OutputFallbackReason::DeviceIdentityExpired,
                    requested_mode: self.output.output_settings.mode,
                    actual_mode: None,
                });
            }
        }
        self.configure_output_device();
        let _ = self.refresh_output_devices();
        self.output.output_discovery_lost = false;
        self.output.output_discovery_arrived = false;
        // The target UID is registered before the first capability query.
        if matches!(
            self.output.output_settings.selection,
            OutputDeviceSelection::Device { .. }
        ) && let Err(error) = self.engine.default_output_format()
        {
            self.output.output_selection_failure = Some(OutputFailureInfo {
                code: ErrorCode::OutputDeviceUnavailable,
                message: error.to_string(),
                retryable: true,
            });
        }
    }
}
