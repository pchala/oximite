use super::*;
use oximite_proto::model::PidSettings;

#[test]
fn anti_windup_and_d_on_measurement() {
    let gains = PidSettings {
        kp: 10.0,
        ki: 5.0,
        kd: 2.0,
    };
    let mut pid = PidController::new(&gains);

    // First call seeds prev_measurement so no derivative kick on setpoint jump
    let out1 = pid.update(90.0, 20.0, 0.02);
    assert_eq!(out1, 100.0);

    // When saturated at 100 with positive error, integral does not wind up past 0
    for _ in 0..50 {
        assert_eq!(pid.update(90.0, 20.0, 0.1), 100.0);
    }

    // Once measurement crosses above target, output responds immediately without huge windup
    let out_over = pid.update(90.0, 91.0, 0.1);
    assert!(out_over < 10.0);
}

#[test]
fn reset_if_reactivated_only_clears_on_zero_to_nonzero() {
    let gains = PidSettings {
        kp: 1.0,
        ki: 10.0,
        kd: 0.0,
    };
    let mut pid = PidController::new(&gains);
    pid.update(10.0, 5.0, 0.1);
    let _ = pid.update(10.0, 5.0, 0.5); // builds i_term

    pid.reset_if_reactivated(10.0, 12.0);
    let kept = pid.update(10.0, 10.0, 0.5);
    assert!(kept > 0.0);

    pid.reset_if_reactivated(0.0, 92.0);
    let cleared = pid.update(92.0, 92.0, 0.5);
    assert_eq!(cleared, 0.0);
}
