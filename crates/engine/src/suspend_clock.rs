//! Elapsed time including system suspend, independent of the civil clock.
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Implementations must be monotonic, report suspend coverage, and be safe on an audio callback
/// (no allocation, locks or unbounded work). The epoch is private to the implementation.
pub trait SuspendClock: Send + Sync {
    fn now_ms(&self) -> u64;
    /// False for a limited-precision fallback that excludes suspend. Hosts should
    /// inject a suspend-aware implementation where the native clock is unavailable.
    fn includes_suspend(&self) -> bool;
}

/// Portable fallback. Keeps core usable when no suspend-aware native clock is
/// available, while explicitly reporting reduced timer precision.
pub struct MonotonicPlaybackClock(std::time::Instant);

impl Default for MonotonicPlaybackClock {
    fn default() -> Self {
        Self(std::time::Instant::now())
    }
}

impl SuspendClock for MonotonicPlaybackClock {
    fn now_ms(&self) -> u64 {
        self.0.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
    }
    fn includes_suspend(&self) -> bool {
        false
    }
}

/// Native suspend-aware clock. Construction verifies platform availability.
pub struct SystemSuspendClock {
    #[cfg(target_os = "macos")]
    timebase: (u32, u32),
}

impl SystemSuspendClock {
    pub fn new() -> std::io::Result<Self> {
        #[cfg(target_os = "linux")]
        {
            let mut time = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            // SAFETY: time points to writable storage of the ABI's timespec type.
            if unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut time) } != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(Self {})
        }
        #[cfg(target_os = "windows")]
        {
            Ok(Self {})
        }
        #[cfg(target_os = "macos")]
        {
            let mut info = Timebase { numer: 0, denom: 0 };
            // SAFETY: the OS fills this correctly sized timebase structure.
            if unsafe { mach_timebase_info(&mut info) } != 0 || info.denom == 0 {
                return Err(std::io::Error::other("mach timebase unavailable"));
            }
            Ok(Self {
                timebase: (info.numer, info.denom),
            })
        }
        #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
        {
            Err(std::io::Error::other(
                "no suspend-aware clock for this platform",
            ))
        }
    }
}

impl SuspendClock for SystemSuspendClock {
    fn includes_suspend(&self) -> bool {
        true
    }
    fn now_ms(&self) -> u64 {
        #[cfg(target_os = "linux")]
        {
            let mut time = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            // SAFETY: same verified clock and storage as in new(). Never fall back to
            // CLOCK_MONOTONIC: a runtime clock failure closes the deadline gate.
            if unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut time) } != 0 {
                return u64::MAX;
            }
            (time.tv_sec as u64).saturating_mul(1000) + time.tv_nsec as u64 / 1_000_000
        }
        #[cfg(target_os = "windows")]
        {
            // SAFETY: GetTickCount64 has no arguments and includes time in sleep.
            unsafe { GetTickCount64() }
        }
        #[cfg(target_os = "macos")]
        {
            // SAFETY: mach_continuous_time has no arguments; unlike absolute time it
            // continues through system sleep. The timebase was verified in new().
            let ticks = unsafe { mach_continuous_time() };
            (u128::from(ticks) * u128::from(self.timebase.0)
                / u128::from(self.timebase.1)
                / 1_000_000) as u64
        }
        #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
        {
            u64::MAX
        }
    }
}

#[cfg(target_os = "windows")]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetTickCount64() -> u64;
}

#[cfg(target_os = "macos")]
#[repr(C)]
struct Timebase {
    numer: u32,
    denom: u32,
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn mach_continuous_time() -> u64;
    fn mach_timebase_info(info: *mut Timebase) -> i32;
}

/// A callback gate shared with core. Zero means disarmed. Core must freeze output
/// before consuming an expired deadline, so a resumed callback cannot leak new audio.
pub struct PlaybackDeadline {
    clock: Arc<dyn SuspendClock>,
    deadline: AtomicU64,
}

impl PlaybackDeadline {
    pub fn new(clock: Arc<dyn SuspendClock>) -> Self {
        Self {
            clock,
            deadline: AtomicU64::new(0),
        }
    }
    pub fn set(&self, deadline: Option<u64>) {
        self.deadline
            .store(deadline.unwrap_or(0), Ordering::Release);
    }
    pub fn expired(&self) -> bool {
        let deadline = self.deadline.load(Ordering::Acquire);
        deadline != 0 && self.clock.now_ms() >= deadline
    }
}
