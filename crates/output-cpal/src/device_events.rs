//! Native device observations, without playback policy.
#[cfg(target_os = "macos")]
pub(crate) use crate::macos::device_events::{DeviceMonitor, StreamErrors};
#[cfg(windows)]
pub(crate) use crate::windows::device_events::DeviceMonitor;

pub(crate) mod identity;
