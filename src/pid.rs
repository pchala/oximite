//! Generic PID controller shared by the pressure and temperature loops.

use embassy_time::Instant;

pub struct PidController {
    kp: f32,
    ki: f32,
    kd: f32,
    i_term: f32,
    prev_measurement: f32,
    last_time: Option<Instant>,
}

impl PidController {
    pub fn new(kp: f32, ki: f32, kd: f32) -> Self {
        Self {
            kp,
            ki,
            kd,
            i_term: 0.0,
            prev_measurement: 0.0,
            last_time: None,
        }
    }
    pub fn reset(&mut self) {
        self.i_term = 0.0;
        self.last_time = None;
    }
    /// Resets the integral term when the setpoint activates from idle (0 ->
    /// non-zero), so a fresh engagement doesn't inherit stale wind-up.
    pub fn reset_if_reactivated(&mut self, previous: f32, next: f32) {
        if previous == 0.0 && next != 0.0 {
            self.reset();
        }
    }
    pub fn set_coeffs(&mut self, kp: f32, ki: f32, kd: f32) {
        self.kp = kp;
        self.ki = ki;
        self.kd = kd;
    }
    /// `target` is provided by the caller on every call rather than stored
    /// internally, since callers already track the current setpoint locally
    /// (and may recompute it, e.g. flow-limiting, right before each update).
    pub fn update(&mut self, target: f32, measurement: f32) -> f32 {
        const OUTPUT_MAX: f32 = 100.0;
        let now = Instant::now();
        let dt = if let Some(last) = self.last_time {
            (now.duration_since(last).as_micros() as f32 / 1_000_000.0).clamp(0.01, 2.0)
        } else {
            self.prev_measurement = measurement;
            0.02 // default to 50Hz for first cycle
        };
        self.last_time = Some(now);

        let error = target - measurement;

        // D on measurement with correct sign: opposes rate of change
        let d_term = self.kd * (self.prev_measurement - measurement) / dt;
        self.prev_measurement = measurement;

        let p_term = self.kp * error;

        let base_output = p_term + d_term;
        let ideal_output = base_output + self.i_term;

        // Conditional integration (anti-windup): only block the direction that
        // would *worsen* saturation, always allow unwinding back into range.
        let delta = self.ki * error * dt;
        let would_worsen_high = ideal_output >= OUTPUT_MAX && delta > 0.0;
        let would_worsen_low = ideal_output <= 0.0 && delta < 0.0;
        if !would_worsen_high && !would_worsen_low {
            self.i_term = (self.i_term + delta).clamp(-20.0, 100.0);
        }

        (base_output + self.i_term).clamp(0.0, OUTPUT_MAX)
    }
}
