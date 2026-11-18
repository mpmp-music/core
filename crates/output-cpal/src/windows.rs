//! Windows activation and native device observations.
#[cfg(windows)]
use crate::*;
#[cfg(windows)]
pub(crate) use ::windows::Win32;
#[cfg(windows)]
pub(crate) mod activation;
#[cfg(windows)]
pub(crate) mod device_events;
mod route;
use crate::device_events::identity;
