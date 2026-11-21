use super::*;

#[test]
fn backoff_grows_and_caps_with_jitter() {
    assert_eq!(backoff_delay_ms(0, 1_000, 30_000, 0), 1_000);
    assert_eq!(backoff_delay_ms(1, 1_000, 30_000, 50), 2_050);
    assert_eq!(backoff_delay_ms(5, 1_000, 30_000, 0), 30_000);
    assert_eq!(backoff_delay_ms(100, 1_000, 30_000, 249), 30_249);
    assert_eq!(default_backoff_ms(0, 250), 1_000);
}

#[test]
fn unarmed_tracker_never_triggers_rejoin() {
    let mut tracker = GatewayProbeTracker::default();
    assert!(!tracker.is_armed());
    for _ in 0..10 {
        assert!(!tracker.on_probe_timeout());
    }
}

#[test]
fn armed_tracker_triggers_on_third_consecutive_timeout() {
    let mut tracker = GatewayProbeTracker::new();
    tracker.on_probe_ok();
    assert!(tracker.is_armed());

    assert!(!tracker.on_probe_timeout());
    assert!(!tracker.on_probe_timeout());
    assert!(tracker.on_probe_timeout());
}

#[test]
fn lan_activity_and_ok_probe_reset_miss_counter() {
    let mut tracker = GatewayProbeTracker::new();
    tracker.on_probe_ok();

    assert!(!tracker.on_probe_timeout());
    assert!(!tracker.on_probe_timeout());
    tracker.on_lan_activity();
    assert!(!tracker.on_probe_timeout());
    assert!(!tracker.on_probe_timeout());

    tracker.on_probe_ok();
    assert!(!tracker.on_probe_timeout());
    assert!(!tracker.on_probe_timeout());
    assert!(tracker.on_probe_timeout());
}

#[test]
fn recent_activity_respects_interval() {
    assert!(!GatewayProbeTracker::is_recent_activity(10, 0));
    assert!(GatewayProbeTracker::is_recent_activity(
        40,
        40 - (GATEWAY_PROBE_INTERVAL_S - 1)
    ));
    assert!(!GatewayProbeTracker::is_recent_activity(
        40,
        40 - GATEWAY_PROBE_INTERVAL_S
    ));
    assert!(!GatewayProbeTracker::is_recent_activity(10, 20));
}
