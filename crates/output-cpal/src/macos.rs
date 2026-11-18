//! CoreAudio timing, native observations and route facts.
#[cfg(target_os = "macos")]
pub(crate) mod device_events;
mod route;
#[cfg(target_os = "macos")]
pub(crate) mod timing;
#[cfg(target_os = "macos")]
use crate::device_events::identity;
#[cfg(target_os = "macos")]
use route as macos_route;
