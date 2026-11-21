//! Pure PID controller shared by the pump and temperature loops.

use oximite_proto::model::PidSettings;

pub struct PidController {
    kp: f32,
    ki: f32,
    kd: f32,
    i_term: f32,
    prev_measurement: f32,
    initialized: bool,
}

impl PidController {
    pub fn new(gains: &PidSettings) -> Self {
        Self {
            kp: gains.kp,
            ki: gains.ki,
            kd: gains.kd,
            i_term: 0.0,
            prev_measurement: 0.0,
            initialized: false,
        }
    }

    pub fn reset(&mut self) {
        self.i_term = 0.0;
        self.initialized = false;
    }

    /// Resets the integral term when the setpoint activates from idle (0 -> non-zero).
    pub fn reset_if_reactivated(&mut self, previous: f32, next: f32) {
        if previous == 0.0 && next != 0.0 {
            self.reset();
        }
    }

    pub fn set_coeffs(&mut self, gains: &PidSettings) {
        self.kp = gains.kp;
        self.ki = gains.ki;
        self.kd = gains.kd;
    }

    /// Computes control output clamped to `0.0..=100.0` given explicit `dt_s`.
    pub fn update(&mut self, target: f32, measurement: f32, dt_s: f32) -> f32 {
        const OUTPUT_MAX: f32 = 100.0;
        let dt = if self.initialized {
            dt_s.clamp(0.01, 2.0)
        } else {
            self.prev_measurement = measurement;
            self.initialized = true;
            0.02
        };

        let error = target - measurement;
        let d_term = self.kd * (self.prev_measurement - measurement) / dt;
        self.prev_measurement = measurement;

        let p_term = self.kp * error;
        let base_output = p_term + d_term;
        let ideal_output = base_output + self.i_term;

        let delta = self.ki * error * dt;
        let would_worsen_high = ideal_output >= OUTPUT_MAX && delta > 0.0;
        let would_worsen_low = ideal_output <= 0.0 && delta < 0.0;
        if !would_worsen_high && !would_worsen_low {
            self.i_term = (self.i_term + delta).clamp(-20.0, 100.0);
        }

        (base_output + self.i_term).clamp(0.0, OUTPUT_MAX)
    }
}

#[cfg(test)]
#[path = "../../host-tests/src/tests/pid.rs"]
mod tests;
