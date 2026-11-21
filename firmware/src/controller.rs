//! Pure 50 Hz AC-sync control engine: pump phase-angle lookup, 2D pressure/flow
//! arbitration, temperature PID with flow feed-forward, and delta-sigma heater
//! modulation.

use oximite_proto::model::{ControlSettings, PumpMode, Telemetry, STEP_NONE};

use crate::pid::PidController;

pub const W_PRESSURE: f32 = 4.0;
pub const W_FLOW: f32 = 1.0;
pub const CONST_FF: f32 = 0.021;

/// Inverse of delivered AC half-wave sine power, sampled at 1% steps from 0%
/// (0.80 half-wave delay) to 100% (0.20 half-wave delay).
pub const POWER_TO_DELAY_LUT: [f32; 101] = [
    0.8000, 0.7876, 0.7763, 0.7659, 0.7562, 0.7471, 0.7385, 0.7302, 0.7224, 0.7148, 0.7076, 0.7005,
    0.6937, 0.6871, 0.6807, 0.6744, 0.6683, 0.6623, 0.6564, 0.6507, 0.6450, 0.6395, 0.6340, 0.6286,
    0.6233, 0.6181, 0.6130, 0.6078, 0.6028, 0.5978, 0.5929, 0.5880, 0.5831, 0.5783, 0.5735, 0.5688,
    0.5640, 0.5594, 0.5547, 0.5501, 0.5454, 0.5408, 0.5363, 0.5317, 0.5271, 0.5226, 0.5181, 0.5135,
    0.5090, 0.5045, 0.5000, 0.4955, 0.4910, 0.4865, 0.4819, 0.4774, 0.4729, 0.4683, 0.4637, 0.4592,
    0.4546, 0.4499, 0.4453, 0.4406, 0.4360, 0.4312, 0.4265, 0.4217, 0.4169, 0.4120, 0.4071, 0.4022,
    0.3972, 0.3922, 0.3870, 0.3819, 0.3767, 0.3714, 0.3660, 0.3605, 0.3550, 0.3493, 0.3436, 0.3377,
    0.3317, 0.3256, 0.3193, 0.3129, 0.3063, 0.2995, 0.2924, 0.2852, 0.2776, 0.2698, 0.2615, 0.2529,
    0.2438, 0.2341, 0.2237, 0.2124, 0.2000,
];

pub fn delay_fraction(power_percent: f32) -> f32 {
    let p = power_percent.clamp(0.0, 100.0);
    let index = p as usize;
    if index >= 100 {
        return POWER_TO_DELAY_LUT[100];
    }
    let remainder = p - (index as f32);
    let lower = POWER_TO_DELAY_LUT[index];
    let upper = POWER_TO_DELAY_LUT[index + 1];
    lower + (upper - lower) * remainder
}

/// Coherent control-loop command handoff from coordinator to control loop.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ControlIntent {
    pub target_temp: f32,
    pub pump_mode: PumpMode,
    pub brew_active: bool,
    pub step_idx: u8,
}

impl Default for ControlIntent {
    fn default() -> Self {
        Self {
            target_temp: 20.0,
            pump_mode: PumpMode::Idle,
            brew_active: false,
            step_idx: STEP_NONE,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ControlInput {
    pub ac_period_us: Option<u32>,
    pub dt_s: f32,
    pub pressure_bar: f32,
    pub temp_c: f32,
    pub flow_ml_s: f32,
    pub volume_ml: f32,
    pub intent: ControlIntent,
    pub settings: ControlSettings,
    pub settings_rev: u32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ControlOutput {
    pub triac_delay_us: Option<u32>,
    pub fire_heater: bool,
    pub telemetry: Telemetry,
}

pub struct ControlEngine {
    ac_ema: f32,
    pump_pid: PidController,
    temp_pid: PidController,
    mode: PumpMode,
    target_t: f32,
    brew_active: bool,
    feed_forward: f32,
    last_settings_rev: u32,
    tick: u32,
    heater_duty: f32,
    heater_accumulator: f32,
    temp_dt_accum: f32,
}

impl ControlEngine {
    pub fn new(initial: ControlSettings, settings_rev: u32) -> Self {
        let target_t = initial.machine.brew_temp;
        let feed_forward = compute_feed_forward(&initial, target_t);
        Self {
            ac_ema: 10_000.0,
            pump_pid: PidController::new(&initial.pump_pid),
            temp_pid: PidController::new(&initial.temp_pid),
            mode: PumpMode::Idle,
            target_t,
            brew_active: false,
            feed_forward,
            last_settings_rev: settings_rev,
            tick: 0,
            heater_duty: 0.0,
            heater_accumulator: 0.0,
            temp_dt_accum: 0.0,
        }
    }

    pub fn tick(&mut self, input: &ControlInput) -> ControlOutput {
        if let Some(raw_us) = input.ac_period_us {
            let half_wave_us = raw_us as f32;
            if half_wave_us > 7_500.0 && half_wave_us < 11_500.0 {
                const ALPHA_AC: f32 = 0.10;
                self.ac_ema += ALPHA_AC * (half_wave_us - self.ac_ema);
            }
        }

        let settings_changed = input.settings_rev != self.last_settings_rev;
        if settings_changed {
            self.last_settings_rev = input.settings_rev;
            self.pump_pid.set_coeffs(&input.settings.pump_pid);
            self.temp_pid.set_coeffs(&input.settings.temp_pid);
            self.feed_forward = compute_feed_forward(&input.settings, input.intent.target_temp);
        }

        if input.intent.pump_mode != self.mode {
            self.pump_pid.set_coeffs(&input.settings.pump_pid);
            if !self.mode.has_target() && input.intent.pump_mode.has_target() {
                self.pump_pid.reset();
            }
            self.mode = input.intent.pump_mode;
        }
        self.brew_active = input.intent.brew_active;

        if (input.intent.target_temp - self.target_t).abs() > f32::EPSILON || settings_changed {
            self.temp_pid.set_coeffs(&input.settings.temp_pid);
            self.temp_pid
                .reset_if_reactivated(self.target_t, input.intent.target_temp);
            self.target_t = input.intent.target_temp;
            self.feed_forward = compute_feed_forward(&input.settings, self.target_t);
        }

        let (target_p, target_q) = self.mode.targets();
        let mut flow_controlled = false;
        let p_output: f32 = match self.mode {
            PumpMode::DirectPump(dp) => dp.clamp(0.0, 100.0),
            _ if self.mode.has_target() => {
                let u_p = if target_p > 0.0 {
                    1.0 + W_PRESSURE * (input.pressure_bar / target_p - 1.0)
                } else {
                    f32::NEG_INFINITY
                };
                let u_q = if target_q > 0.0 {
                    1.0 + W_FLOW * (input.flow_ml_s / target_q - 1.0)
                } else {
                    f32::NEG_INFINITY
                };
                flow_controlled = u_q >= u_p;
                self.pump_pid.update(1.0, u_q.max(u_p), input.dt_s)
            }
            _ => 0.0,
        };

        let triac_delay_us = if p_output > 0.0 {
            Some((delay_fraction(p_output) * self.ac_ema) as u32)
        } else {
            None
        };

        let effective_target_t = if self.brew_active {
            self.target_t + (input.flow_ml_s * self.feed_forward).clamp(0.0, 20.0)
        } else {
            self.target_t
        };

        self.temp_dt_accum += input.dt_s.max(0.0);
        if self.tick.is_multiple_of(10) {
            let dt = if self.tick == 0 {
                0.02
            } else {
                self.temp_dt_accum
            };
            self.heater_duty = self.temp_pid.update(effective_target_t, input.temp_c, dt);
            self.temp_dt_accum = 0.0;
        }

        self.heater_accumulator += self.heater_duty;
        let fire_heater = if self.heater_accumulator >= 100.0 {
            self.heater_accumulator -= 100.0;
            true
        } else {
            false
        };

        let telemetry = Telemetry {
            tick: self.tick,
            pressure_bar: input.pressure_bar,
            temp_c: input.temp_c,
            target_bar: target_p,
            target_temp: self.target_t,
            effective_target_temp: effective_target_t,
            target_ml_s: target_q,
            flow_rate_ml_s: input.flow_ml_s,
            volume_ml: input.volume_ml,
            flow_controlled,
            heater_duty: self.heater_duty,
            pump_duty: p_output,
            ac_period_us: self.ac_ema,
        };

        self.tick = self.tick.wrapping_add(1);

        ControlOutput {
            triac_delay_us,
            fire_heater,
            telemetry,
        }
    }
}

fn compute_feed_forward(s: &ControlSettings, target_t: f32) -> f32 {
    CONST_FF * (s.machine.feed_forward_percents / 100.0) * (target_t - 20.0)
}

#[cfg(test)]
#[path = "../../host-tests/src/tests/controller.rs"]
mod tests;
