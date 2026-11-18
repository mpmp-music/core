use super::*;

/// A seek far ahead (the duration unknown) must not overflow once time passes
/// (debug builds panicked the core thread, release builds wrapped).
#[test]
fn seek_hints_saturate() {
    let hint = SeekHint {
        instance: 1,
        position_ms: u64::MAX - 5,
        at_ms: 0,
        running: true,
    };
    assert_eq!(hint.at(10), u64::MAX);
    let paused = SeekHint {
        running: false,
        ..hint
    };
    assert_eq!(paused.at(10), u64::MAX - 5);
}

mod output;
