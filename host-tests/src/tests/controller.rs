use super::*;
use oximite_proto::model::{ControlSettings, MachineSettings, PidSettings, DEFAULT_SETTINGS};

fn base_input(intent: ControlIntent, settings: ControlSettings, rev: u32) -> ControlInput {
    ControlInput {
        ac_period_us: Some(10_000),
        dt_s: 0.02,
        pressure_bar: 0.0,
        temp_c: 92.0,
        flow_ml_s: 0.0,
        volume_ml: 0.0,
        intent,
        settings,
        settings_rev: rev,
    }
}

#[test]
fn delay_lut_endpoints_and_monotonic_decreasing() {
    assert!((delay_fraction(0.0) - 0.8000).abs() < 1e-4);
    assert!((delay_fraction(50.0) - 0.5000).abs() < 1e-4);
    assert!((delay_fraction(100.0) - 0.2000).abs() < 1e-4);
    assert!((delay_fraction(-10.0) - 0.8000).abs() < 1e-4);
    assert!((delay_fraction(150.0) - 0.2000).abs() < 1e-4);
    for p in 0..100 {
        assert!(delay_fraction(p as f32) > delay_fraction((p + 1) as f32));
    }
}

#[test]
fn ac_ema_tracks_60hz_and_ignores_glitches() {
    let s = ControlSettings::default();
    let mut eng = ControlEngine::new(s, 1);
    let intent = ControlIntent::default();
    let mut inp = base_input(intent, s, 1);

    inp.ac_period_us = Some(8_333);
    for _ in 0..100 {
        eng.tick(&inp);
    }
    let out_60hz = eng.tick(&inp);
    assert!((out_60hz.telemetry.ac_period_us - 8_333.0).abs() < 1.0);

    // Outlier outside 7500..11500 must be ignored
    inp.ac_period_us = Some(2_000);
    let out_glitch = eng.tick(&inp);
    assert!((out_glitch.telemetry.ac_period_us - out_60hz.telemetry.ac_period_us).abs() < 1e-3);
}

#[test]
fn unified_2d_pump_arbitration_picks_binding_constraint() {
    let s = ControlSettings::default();
    let mut eng = ControlEngine::new(s, 1);
    let intent = ControlIntent {
        target_temp: 92.0,
        pump_mode: PumpMode::Unified {
            target_bar: 9.0,
            target_ml_s: 2.5,
        },
        brew_active: true,
        step_idx: 0,
    };

    // High flow, low pressure -> flow is binding
    let mut inp = base_input(intent, s, 1);
    inp.pressure_bar = 2.0;
    inp.flow_ml_s = 3.0;
    let out = eng.tick(&inp);
    assert!(out.telemetry.flow_controlled);

    // High pressure, low flow -> pressure is binding
    inp.pressure_bar = 9.5;
    inp.flow_ml_s = 1.0;
    let out2 = eng.tick(&inp);
    assert!(!out2.telemetry.flow_controlled);

    // Pressure-only target (target_ml_s = 0.0) -> pressure always governs
    inp.intent.pump_mode = PumpMode::Unified {
        target_bar: 9.0,
        target_ml_s: 0.0,
    };
    inp.pressure_bar = 4.0;
    inp.flow_ml_s = 5.0;
    let out_p_only = eng.tick(&inp);
    assert!(!out_p_only.telemetry.flow_controlled);
    assert!(out_p_only.triac_delay_us.is_some());

    // Flow-only target (target_bar = 0.0) -> flow always governs
    inp.intent.pump_mode = PumpMode::Unified {
        target_bar: 0.0,
        target_ml_s: 2.5,
    };
    inp.pressure_bar = 9.0;
    inp.flow_ml_s = 1.0;
    let out_q_only = eng.tick(&inp);
    assert!(out_q_only.telemetry.flow_controlled);
    assert!(out_q_only.triac_delay_us.is_some());
}

#[test]
fn brew_flow_feed_forward_boosts_only_during_brew() {
    let s = ControlSettings::default();
    let mut eng = ControlEngine::new(s, 1);
    let mut intent = ControlIntent {
        target_temp: 100.0,
        pump_mode: PumpMode::DirectPump(50.0),
        brew_active: false,
        step_idx: 0,
    };
    let mut inp = base_input(intent, s, 1);
    inp.flow_ml_s = 4.0;

    let no_brew = eng.tick(&inp);
    assert!((no_brew.telemetry.effective_target_temp - 100.0).abs() < 1e-4);

    intent.brew_active = true;
    inp.intent = intent;
    let during_brew = eng.tick(&inp);
    assert!(during_brew.telemetry.effective_target_temp > 105.0);
    assert!(during_brew.telemetry.effective_target_temp <= 120.0);
}

#[test]
fn heater_delta_sigma_matches_average_duty() {
    let s = ControlSettings {
        machine: MachineSettings {
            brew_temp: 92.0,
            ..DEFAULT_SETTINGS.machine
        },
        temp_pid: PidSettings {
            kp: 10.0,
            ki: 0.0,
            kd: 0.0,
        },
        pump_pid: DEFAULT_SETTINGS.pump_pid,
    };
    let mut eng = ControlEngine::new(s, 1);
    let intent = ControlIntent {
        target_temp: 92.0,
        pump_mode: PumpMode::Idle,
        brew_active: false,
        step_idx: u8::MAX,
    };
    let mut inp = base_input(intent, s, 1);
    inp.temp_c = 89.0; // error 3 * kp 10 = 30% duty

    let mut fired = 0usize;
    for _ in 0..100 {
        if eng.tick(&inp).fire_heater {
            fired += 1;
        }
    }
    assert_eq!(fired, 30);
}

#[test]
fn settings_revision_updates_feed_forward_without_restart() {
    let mut s = ControlSettings::default();
    let mut eng = ControlEngine::new(s, 1);
    let intent = ControlIntent {
        target_temp: 100.0,
        pump_mode: PumpMode::Idle,
        brew_active: true,
        step_idx: 0,
    };
    let mut inp = base_input(intent, s, 1);
    inp.flow_ml_s = 3.0;
    let out1 = eng.tick(&inp);
    assert!(out1.telemetry.effective_target_temp > 104.0);

    // Disable feed-forward in rev 2
    s.machine.feed_forward_percents = 0.0;
    inp.settings = s;
    inp.settings_rev = 2;
    let out2 = eng.tick(&inp);
    assert!((out2.telemetry.effective_target_temp - 100.0).abs() < 1e-4);
}
