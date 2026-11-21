//! Pure flow-turbine rate averaging and volume accumulation.

use oximite_proto::model::DEFAULT_SETTINGS;

use crate::board::SYS_CLK_HZ;

pub const CYCLES_PER_LOOP: f32 = 2.0;

/// Counts added to each PIO half-period to account for the 4 cycles per pulse
/// spent outside the counting loops, split evenly between HIGH and LOW.
pub const PIO_TICK_OFFSET: u32 = 2;

/// Number of half-period samples averaged for instantaneous flow rate.
pub const RATE_EDGES: usize = 4;

fn effective_pulses_per_liter(pulses_per_liter: f32) -> f32 {
    if pulses_per_liter > 0.0 {
        pulses_per_liter
    } else {
        DEFAULT_SETTINGS.machine.flow_pulses_per_liter
    }
}

/// Sliding window of the most recent half-period measurements plus shot volume.
pub struct FlowTracker {
    ticks: [u32; RATE_EDGES],
    head: usize,
    len: usize,
    ml_per_pulse: f32,
    flow_numerator: f32,
    rate_ml_s: f32,
    volume_ml: f32,
}

impl FlowTracker {
    pub fn new(pulses_per_liter: f32) -> Self {
        let mut s = Self {
            ticks: [0; RATE_EDGES],
            head: 0,
            len: 0,
            ml_per_pulse: 0.0,
            flow_numerator: 0.0,
            rate_ml_s: 0.0,
            volume_ml: 0.0,
        };
        s.set_pulses_per_liter(pulses_per_liter);
        s
    }

    pub fn set_pulses_per_liter(&mut self, pulses_per_liter: f32) {
        let ppl = effective_pulses_per_liter(pulses_per_liter);
        self.ml_per_pulse = 1000.0 / ppl;
        self.flow_numerator = (SYS_CLK_HZ / CYCLES_PER_LOOP) * self.ml_per_pulse;
    }

    pub fn rate_ml_s(&self) -> f32 {
        self.rate_ml_s
    }

    pub fn volume_ml(&self) -> f32 {
        self.volume_ml
    }

    /// Ingests one raw half-period count from PIO2 SM0 and returns the updated flow rate.
    pub fn on_edge(&mut self, raw_ticks: u32) -> f32 {
        let adjusted = raw_ticks.saturating_add(PIO_TICK_OFFSET);
        self.ticks[self.head] = adjusted;
        self.head = (self.head + 1) % RATE_EDGES;
        self.len = (self.len + 1).min(RATE_EDGES);

        let sum: u64 = self.ticks[..self.len].iter().map(|&t| u64::from(t)).sum();
        if sum > 0 {
            self.rate_ml_s = self.flow_numerator * self.len as f32 / sum as f32;
        }
        self.volume_ml += self.ml_per_pulse;
        self.rate_ml_s
    }

    /// Called when no turbine edge arrives within 200 ms: clears rate window, keeps volume.
    pub fn on_timeout(&mut self) {
        self.head = 0;
        self.len = 0;
        self.rate_ml_s = 0.0;
    }

    /// Zeroes both accumulated shot volume and flow rate window.
    pub fn reset_volume(&mut self) {
        self.on_timeout();
        self.volume_ml = 0.0;
    }
}

#[cfg(test)]
#[path = "../../host-tests/src/tests/flow.rs"]
mod tests;
