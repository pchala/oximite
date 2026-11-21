use super::*;

#[test]
fn flow_rate_and_volume_accumulate_and_timeout() {
    let mut tracker = FlowTracker::new(100_000.0); // 0.01 ml/edge
                                                   // 10 edges/sec => 15_000_000 cycles per edge => ~0.1 ml/s
    let half_period_cycles = 7_500_000u32 - PIO_TICK_OFFSET;
    for _ in 0..4 {
        tracker.on_edge(half_period_cycles);
    }
    assert!((tracker.rate_ml_s() - 0.1).abs() < 1e-4);
    assert!((tracker.volume_ml() - 0.04).abs() < 1e-5);

    // Timeout drops rate to 0 but preserves shot volume
    tracker.on_timeout();
    assert_eq!(tracker.rate_ml_s(), 0.0);
    assert!((tracker.volume_ml() - 0.04).abs() < 1e-5);

    tracker.reset_volume();
    assert_eq!(tracker.volume_ml(), 0.0);
}

#[test]
fn dynamic_calibration_update_takes_effect_immediately() {
    let mut tracker = FlowTracker::new(50_000.0);
    tracker.on_edge(1_000_000);
    let v1 = tracker.volume_ml();
    tracker.set_pulses_per_liter(100_000.0);
    tracker.on_edge(1_000_000);
    let v2 = tracker.volume_ml() - v1;
    assert!((v1 - 2.0 * v2).abs() < 1e-5);

    // Non-positive calibration falls back to DEFAULT_SETTINGS.machine.flow_pulses_per_liter
    let mut fallback = FlowTracker::new(0.0);
    fallback.on_edge(1_000_000);
    let expected_ml = 1000.0 / DEFAULT_SETTINGS.machine.flow_pulses_per_liter;
    assert!((fallback.volume_ml() - expected_ml).abs() < 1e-6);
}
