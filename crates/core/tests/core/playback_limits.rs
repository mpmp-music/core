//! Playback limits scenarios.
use super::*;

/// `limit: 0` only counts; `next_offset` never points at an empty page.
#[test]
fn zero_limit_counts_and_next_offset_ends() {
    let mut h = Harness::new(None);
    h.ok(play(album()));
    let query = |json: &str| -> serde_json::Value {
        serde_json::from_str(&h.core.query_json(json)).unwrap()
    };
    let r = query(r#"{"type":"queue_page","limit":0}"#);
    assert_eq!(
        (&r["total"], &r["next_offset"], &r["entries"]),
        (
            &serde_json::json!(3),
            &serde_json::Value::Null,
            &serde_json::json!([])
        ),
        "{r}"
    );
    let r = query(r#"{"type":"queue_page","offset":1,"limit":1}"#);
    assert_eq!(r["next_offset"], 2, "{r}");
    let r = query(r#"{"type":"queue_page","offset":2,"limit":5}"#);
    assert_eq!(r["next_offset"], serde_json::Value::Null, "{r}");
    let r = query(r#"{"type":"queue_page","offset":7,"limit":5}"#);
    assert_eq!(
        (&r["total"], &r["next_offset"]),
        (&serde_json::json!(3), &serde_json::Value::Null),
        "{r}"
    );
}

#[test]
fn playback_limits_are_not_restored_after_a_core_restart() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.json");
    {
        let mut h = Harness::new(Some(path.clone()));
        h.ok(play(album()));
        h.ok(Command::Pause);
        h.ok(Command::SetStopAfterCurrent {
            enabled: true,
            expected_entry: None,
        });
        h.ok(Command::SetSleepTimer {
            duration_ms: Some(MAX_SLEEP_TIMER_MS),
        });
        assert!(
            h.snapshot()
                .playback_limits
                .unwrap()
                .stop_after_current
                .is_some()
        );
        h.core.shutdown();
    }
    let saved = std::fs::read_to_string(&path).unwrap();
    assert!(!saved.contains("playback_limits") && !saved.contains("sleep_timer"));
    let h = Harness::new(Some(path));
    assert_eq!(
        h.snapshot().playback_limits,
        Some(PlaybackLimitsInfo::default())
    );
    assert!(!h.output.is_open());
    match h.core.query(Query::PlaybackLimits) {
        QueryResult::PlaybackLimits {
            playback_limits, ..
        } => assert_eq!(playback_limits, PlaybackLimitsInfo::default()),
        result => panic!("{result:?}"),
    }
}

#[test]
fn playback_limit_scheduler_expires_without_audio_callbacks_or_position_subscribers() {
    use std::sync::atomic::{AtomicU64, Ordering};
    #[derive(Default)]
    struct Clock(AtomicU64);
    impl mpmp_engine::SuspendClock for Clock {
        fn includes_suspend(&self) -> bool {
            true
        }
        fn now_ms(&self) -> u64 {
            self.0.load(Ordering::Acquire)
        }
    }
    let clock = Arc::new(Clock::default());
    let mut h = Harness::with_config(
        Arc::new(MemoryCatalog::standard()),
        CoreConfig {
            playback_clock: Some(clock.clone()),
            automatic_loudness_scan: Some(false),
            ..Default::default()
        },
    );
    let events = h
        .core
        .subscribe(
            Origin::Ui,
            SubscribeOptions::default().with_positions(false),
        )
        .unwrap()
        .events;
    h.ok(Command::SetSleepTimer {
        duration_ms: Some(MAX_SLEEP_TIMER_MS),
    });
    clock.0.store(MAX_SLEEP_TIMER_MS, Ordering::Release);
    loop {
        let event = events
            .recv_timeout(TIMEOUT)
            .expect("timer did not expire without audio callbacks");
        if let Event::PlaybackLimitReached {
            reason,
            action,
            entry,
        } = event.event
        {
            assert_eq!(
                (reason, action, entry),
                (
                    PlaybackLimitReason::SleepTimer,
                    PlaybackLimitAction::Pause,
                    None
                )
            );
            assert_eq!(event.origin, None);
            break;
        }
    }
    assert!(!h.output.is_open());
}
