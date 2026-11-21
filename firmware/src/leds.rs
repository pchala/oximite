//! Pure WS2812 RGB status calculation and PIO word packing.

use oximite_proto::model::{MachineState, Telemetry};

pub const BRIGHTNESS: u32 = 30;
pub const PUMP_TARGET_DUTY: f32 = 80.0;
pub const PUMP_DUTY_WINDOW: f32 = 20.0;
pub const WARMUP_MS: u64 = 6 * 60 * 1000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }
    pub const fn off() -> Self {
        Self::new(0, 0, 0)
    }
}

/// Blue (cold/low) -> Green (on target) -> Red (hot/high), with +/-`window` crossfade.
pub fn temp_color(current: f32, target: f32, window: f32) -> Rgb {
    let (r, g, b) = if current <= target {
        let lower_bound = target - window;
        if current <= lower_bound {
            (0.0_f32, 0.0_f32, 255.0_f32)
        } else {
            let g = 255.0 * (current - lower_bound) / window;
            (0.0, g, 255.0 - g)
        }
    } else {
        let upper_bound = target + window;
        if current >= upper_bound {
            (255.0_f32, 0.0_f32, 0.0_f32)
        } else {
            let r = 255.0 * (current - target) / window;
            (r, 255.0 - r, 0.0)
        }
    };

    Rgb::new(r as u8, g as u8, b as u8)
}

/// Computes `[led0, led1]` colors for the current machine state and telemetry.
pub fn render_leds(state: MachineState, telemetry: &Telemetry, in_warmup: bool) -> [Rgb; 2] {
    let boiler = temp_color(telemetry.temp_c, telemetry.target_temp, 10.0);

    match state {
        MachineState::Sleeping => [Rgb::new(255, 0, 255), Rgb::new(255, 0, 255)],
        MachineState::Brewing => {
            let pump = if telemetry.pump_duty > 0.0 {
                temp_color(telemetry.pump_duty, PUMP_TARGET_DUTY, PUMP_DUTY_WINDOW)
            } else {
                Rgb::off()
            };
            [boiler, pump]
        }
        MachineState::Steaming => [Rgb::off(), boiler],
        _ if in_warmup => {
            let r = (telemetry.heater_duty.clamp(0.0, 100.0) * 2.55) as u8;
            [boiler, Rgb::new(r, 0, 0)]
        }
        _ => [boiler, Rgb::off()],
    }
}

/// Scales `led` by [`BRIGHTNESS`] and packs into WS2812 GRB top-24-bit word.
pub const fn pack_grb(led: Rgb) -> u32 {
    let r = (led.r as u32 * BRIGHTNESS) >> 8;
    let g = (led.g as u32 * BRIGHTNESS) >> 8;
    let b = (led.b as u32 * BRIGHTNESS) >> 8;
    (g << 24) | (r << 16) | (b << 8)
}

#[cfg(test)]
#[path = "../../host-tests/src/tests/leds.rs"]
mod tests;
