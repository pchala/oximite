//! Closed-loop thermal + hydraulic plant simulations running `ControlEngine` +
//! `Machine` + `FlowTracker` at 50 Hz in milliseconds on the host PC.

use oximite_proto::model::{
    BrewProfile, BrewProfileStep, MachineCommand, MachineState, DEFAULT_SETTINGS, MAX_PROFILES,
};

use crate::controller::{ControlEngine, ControlInput};
use crate::flow::FlowTracker;
use crate::machine::Machine;

struct Plant {
    temp_c: f32,
    pressure_bar: f32,
}

impl Plant {
    fn step(&mut self, heater_on: bool, pump_duty: f32, valve_open: bool, dt_s: f32) -> f32 {
        // Simple first-order hydraulic puck model + thermal boiler model
        let target_p = if valve_open {
            (pump_duty / 100.0) * 12.5
        } else {
            0.0
        };
        self.pressure_bar += (target_p - self.pressure_bar) * (dt_s * 6.0).min(1.0);
        let flow_ml_s = if valve_open {
            ((pump_duty / 100.0) * 3.5 - self.pressure_bar * 0.1).max(0.0)
        } else if pump_duty > 0.0 {
            (pump_duty / 100.0) * 4.5
        } else {
            0.0
        };

        let heat_in = if heater_on { 1.6 } else { 0.0 };
        let ambient_loss = (self.temp_c - 22.0) * 0.004;
        let flow_cooling = flow_ml_s * (self.temp_c - 20.0) * 0.015;
        self.temp_c += (heat_in - ambient_loss - flow_cooling) * dt_s;
        flow_ml_s
    }
}

#[test]
fn closed_loop_heatup_and_multistep_profile() {
    let empty: [Option<BrewProfile>; MAX_PROFILES as usize] =
        [const { None }; MAX_PROFILES as usize];
    let mut machine = Machine::new(DEFAULT_SETTINGS, empty, 0);
    let mut engine = ControlEngine::new(machine.control_settings(), machine.settings_rev());
    let mut plant = Plant {
        temp_c: 88.0,
        pressure_bar: 0.0,
    };
    let mut flow = FlowTracker::new(DEFAULT_SETTINGS.machine.flow_pulses_per_liter);

    let mut now_ms: u64 = 0;
    let dt_s = 0.02;

    // 1. Warm up toward 102 C (92 + 10 offset) for 40 simulated seconds
    for _ in 0..2000 {
        now_ms += 20;
        let me = machine.tick(now_ms, flow.volume_ml(), plant.temp_c);
        if me.reset_volume {
            flow.reset_volume();
        }
        let out = engine.tick(&ControlInput {
            ac_period_us: Some(10_000),
            dt_s,
            pressure_bar: plant.pressure_bar,
            temp_c: plant.temp_c,
            flow_ml_s: 0.0,
            volume_ml: flow.volume_ml(),
            intent: machine.intent(),
            settings: machine.control_settings(),
            settings_rev: machine.settings_rev(),
        });
        plant.step(
            out.fire_heater,
            out.telemetry.pump_duty,
            machine.valve_open(),
            dt_s,
        );
    }
    assert!(
        (plant.temp_c - 102.0).abs() < 4.0,
        "boiler temp did not converge near 102 C: {}",
        plant.temp_c
    );

    // 2. Run a 2-step profile: 3s pre-infusion at 3 bar, then 9 bar until 25 ml
    let mut name = heapless::String::new();
    let _ = name.push_str("SimShot");
    let mut steps = heapless::Vec::new();
    let _ = steps.push(BrewProfileStep {
        time_s: Some(3.0),
        volume: None,
        pressure: Some(3.0),
        flow: Some(2.5),
    });
    let _ = steps.push(BrewProfileStep {
        time_s: Some(40.0),
        volume: Some(25.0),
        pressure: Some(9.0),
        flow: Some(3.0),
    });

    let start_eff = machine.handle_command(
        MachineCommand::RunProfile(BrewProfile { name, steps }),
        now_ms,
    );
    assert!(start_eff.reset_volume);
    flow.reset_volume();
    assert_eq!(machine.state(), MachineState::Brewing);
    assert!(machine.valve_open());

    let mut shot_vol = 0.0f32;
    let mut finished = false;
    for _ in 0..2500 {
        now_ms += 20;
        let out = engine.tick(&ControlInput {
            ac_period_us: Some(10_000),
            dt_s,
            pressure_bar: plant.pressure_bar,
            temp_c: plant.temp_c,
            flow_ml_s: (shot_vol > 0.0) as u8 as f32 * 2.0, // simplified flow feedback
            volume_ml: shot_vol,
            intent: machine.intent(),
            settings: machine.control_settings(),
            settings_rev: machine.settings_rev(),
        });
        let f = plant.step(
            out.fire_heater,
            out.telemetry.pump_duty,
            machine.valve_open(),
            dt_s,
        );
        shot_vol += f * dt_s;
        let _ = machine.tick(now_ms, shot_vol, plant.temp_c);
        if machine.state() == MachineState::Idle {
            finished = true;
            break;
        }
    }

    assert!(finished, "shot did not complete within 50 s");
    assert!(!machine.valve_open());
    assert!(shot_vol >= 24.5);
}
