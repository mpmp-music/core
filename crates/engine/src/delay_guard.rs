//! Callback-local delay validation: arithmetic and plain values only.
use std::time::Duration;

// Device queues are independent of our ring. Two seconds accommodates normal
// Bluetooth latency (including a stable one-second lead), while bounding broken
// format-change timestamps. A 250 ms step from the last trusted value is suspect
// even below that ceiling; normal callback jitter is much smaller.
const MAX_DELAY: Duration = Duration::from_secs(2);

const MAX_DELAY_STEP: Duration = Duration::from_millis(250);

fn validate_delay(delay: Option<Duration>, trusted: Option<Duration>) -> (Duration, bool) {
    let value = delay.unwrap_or_default();
    let invalid = value > MAX_DELAY
        || trusted.is_some_and(|previous| value.abs_diff(previous) > MAX_DELAY_STEP);
    if invalid {
        // With no trustworthy timestamp, wait the whole supported device tail
        // rather than declaring completion at the rendered boundary.
        (trusted.unwrap_or(MAX_DELAY), true)
    } else {
        (value, false)
    }
}

#[derive(Default)]
pub(crate) struct DelayGuard {
    trusted: Option<Duration>,
    unreliable: bool,
}

impl DelayGuard {
    pub fn completion_delay(&self) -> Duration {
        if self.unreliable {
            MAX_DELAY
        } else {
            Duration::ZERO
        }
    }

    pub fn check(&mut self, delay: Option<Duration>) -> (Duration, bool) {
        let (safe, invalid) = validate_delay(delay, self.trusted);
        let newly_unreliable = invalid && !self.unreliable;
        self.unreliable = invalid;
        if !invalid {
            self.trusted = delay;
        }
        (safe, newly_unreliable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_bluetooth_delays_are_trusted_from_the_first_callback() {
        for delay in [
            Duration::from_millis(300),
            Duration::from_secs(1),
            MAX_DELAY,
        ] {
            let mut guard = DelayGuard::default();
            for _ in 0..10 {
                assert_eq!(guard.check(Some(delay)), (delay, false));
            }
        }
    }

    #[test]
    fn jumps_inside_and_outside_absolute_limit_keep_last_trusted_delay() {
        for jump in [
            Duration::from_secs(2),
            Duration::from_secs(3),
            Duration::MAX,
        ] {
            let mut guard = DelayGuard::default();
            let good = Duration::from_millis(300);
            assert_eq!(guard.check(Some(good)), (good, false));
            assert_eq!(guard.check(Some(jump)), (good, true));
            assert_eq!(guard.check(Some(jump)), (good, false));
            assert_eq!(guard.check(Some(good)), (good, false));
            assert_eq!(guard.check(Some(jump)), (good, true));
        }
    }

    #[test]
    fn absent_trusted_delay_uses_conservative_tail_and_small_jitter_is_accepted() {
        let mut guard = DelayGuard::default();
        assert_eq!(guard.check(Some(Duration::MAX)), (MAX_DELAY, true));
        let good = Duration::from_secs(1);
        assert_eq!(guard.check(Some(good)), (good, false));
        let jitter = good + Duration::from_millis(10);
        assert_eq!(guard.check(Some(jitter)), (jitter, false));
        assert_eq!(validate_delay(None, None), (Duration::ZERO, false));
    }
}
