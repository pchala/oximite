use embassy_futures::select::{select, Either};
use embassy_futures::yield_now;
use embassy_rp::adc::{Adc, Async, Channel};
use embassy_rp::gpio::Output;
use embassy_rp::peripherals::PIO0;
use embassy_rp::pio::{Common, Config, Direction, Pin, StateMachine};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_sync::watch::Watch;
use embassy_time::{Duration, Instant, Timer};
use fixed::FixedU32;
use pio::pio_asm;

use crate::settings::{BrewProfile, Settings};
use crate::state::{MachineCommand, SIG_COMMAND};

pub static SIG_TARGET_PRESSURE: Signal<CriticalSectionRawMutex, f32> = Signal::new();
pub static SIG_FLOW_LIMIT: Signal<CriticalSectionRawMutex, f32> = Signal::new();
pub static SIG_TARGET_TEMP: Signal<CriticalSectionRawMutex, f32> = Signal::new();
pub static SIG_PROFILE_ABORT: Signal<CriticalSectionRawMutex, ()> = Signal::new();
pub static SIG_DIRECT_PUMP: Signal<CriticalSectionRawMutex, Option<f32>> = Signal::new();

#[derive(Clone)]
#[allow(clippy::large_enum_variant)]
pub enum HardwareCommand {
    RunProfile(BrewProfile),
    Steam,
    Descale,
    DirectPump(f32),
    CooldownFlush,
    HotWater,
}
pub static SIG_HARDWARE_CMD: Signal<CriticalSectionRawMutex, HardwareCommand> = Signal::new();

pub fn set_target_pressure(bar: f32) {
    SIG_TARGET_PRESSURE.signal(bar);
}
pub fn set_flow_limit(ml_s: f32) {
    SIG_FLOW_LIMIT.signal(ml_s);
}
pub enum TargetTempMode {
    Brew,
    Steam,
    Descale,
    Off,
}

pub async fn set_target_temp(mode: TargetTempMode) {
    let s = crate::settings::ControlSettings::current();
    let temp = match mode {
        TargetTempMode::Brew => crate::state::get_session_brew_temp() + s.machine.temp_offset,
        TargetTempMode::Steam => s.machine.steam_temp,
        TargetTempMode::Descale => 60.0,
        TargetTempMode::Off => 0.0,
    };
    SIG_TARGET_TEMP.signal(temp);
}
pub fn set_direct_pump(power: Option<f32>) {
    SIG_DIRECT_PUMP.signal(power);
}

/// Pump power used for flush and cooldown operations (%).
pub const PUMP_POWER: f32 = 80.0;

/// How the pump should be driven: an open-loop power percentage, or a
/// pressure-PID target in bar. These are mutually exclusive in
/// `ac_sync_control_task` — setting one implicitly overrides the other.
pub enum PumpMode {
    DirectPump(f32),
    Pressure(f32),
}

impl PumpMode {
    fn apply(&self) {
        match *self {
            PumpMode::DirectPump(power) => {
                set_target_pressure(0.0);
                set_direct_pump(Some(power));
            }
            PumpMode::Pressure(bar) => {
                set_direct_pump(None);
                set_target_pressure(bar);
            }
        }
    }
}

/// RAII guard for the pump: applies `mode` when created/changed, and always
/// returns the pump to idle (no direct pump, no pressure target) when
/// dropped — including when the enclosing future is cancelled mid-await (as
/// `run_cancellable` does on abort). This guarantees an aborted operation
/// can't leave the pump engaged, without every `execute_*` function having
/// to remember a manual reset at each exit point.
pub struct PumpGuard;

impl PumpGuard {
    pub fn engage(mode: PumpMode) -> Self {
        mode.apply();
        Self
    }

    /// Switches an already-engaged pump to a new mode (e.g. between profile
    /// steps) without affecting the drop-time reset.
    pub fn set_mode(&mut self, mode: PumpMode) {
        mode.apply();
    }
}

impl Drop for PumpGuard {
    fn drop(&mut self) {
        set_direct_pump(None);
        set_target_pressure(0.0);
    }
}

pub static SIG_BREW_ACTIVE: Signal<CriticalSectionRawMutex, bool> = Signal::new();

/// RAII guard marking that a real brew profile (not cooldown flush, descale,
/// hot water, or a raw direct-pump command) is running. `ac_sync_control_task`
/// only substitutes `target` for the real measurement (freezing the PID's
/// output, see the temp-control loop below) while this is armed — those other
/// operations set an explicit temperature target (often `Off`, to let cold
/// water cool the boiler as fast as possible) and must keep tracking it via
/// the normal PID, not have the heater frozen just because the pump is flowing.
pub struct BrewActiveGuard;

impl BrewActiveGuard {
    pub fn engage() -> Self {
        SIG_BREW_ACTIVE.signal(true);
        Self
    }
}

impl Drop for BrewActiveGuard {
    fn drop(&mut self) {
        SIG_BREW_ACTIVE.signal(false);
    }
}

#[derive(Clone, Copy, Default)]
pub struct AdcState {
    pub pressure_bar: f32,
    pub temp_c: f32,
    pub target_bar: f32,
    pub target_temp: f32,
    pub flow_limit_ml_s: f32,
    pub heater_duty: f32,
}

impl AdcState {
    /// Returns `(display_temp, display_target_temp)` with the boiler offset
    /// subtracted for non-steam modes.
    pub fn display_temps(&self, offset: f32, is_steaming: bool) -> (f32, f32) {
        if is_steaming {
            (self.temp_c, self.target_temp)
        } else {
            let t = self.temp_c - offset;
            let tt = if self.target_temp > 0.0 {
                self.target_temp - offset
            } else {
                0.0
            };
            (t, tt)
        }
    }
}

pub static ADC_WATCH: Watch<CriticalSectionRawMutex, AdcState, 4> = Watch::new();

pub struct AdcMonitor;
impl AdcMonitor {
    pub fn new() -> Self {
        Self
    }
    pub async fn get_state(&self) -> AdcState {
        // Fallback to defaults if watch is completely uninitialized
        ADC_WATCH.try_get().unwrap_or(AdcState {
            pressure_bar: 0.0,
            temp_c: 20.0,
            target_bar: 0.0,
            target_temp: 20.0,
            flow_limit_ml_s: 0.0,
            heater_duty: 0.0,
        })
    }
}

const NTC_LUT: [f32; 1025] = [
    300.00, 300.00, 300.00, 300.00, 300.00, 300.00, 300.00, 300.00, 300.00, 295.09, 287.69, 281.17,
    275.29, 270.04, 265.21, 260.83, 256.78, 253.01, 249.51, 246.22, 243.16, 240.27, 237.53, 234.94,
    232.50, 230.15, 227.92, 225.80, 223.77, 221.81, 219.95, 218.14, 216.41, 214.74, 213.13, 211.58,
    210.08, 208.62, 207.21, 205.84, 204.52, 203.23, 201.98, 200.76, 199.57, 198.42, 197.29, 196.19,
    195.12, 194.08, 193.06, 192.06, 191.10, 190.14, 189.21, 188.29, 187.40, 186.52, 185.66, 184.82,
    183.99, 183.18, 182.39, 181.61, 180.84, 180.09, 179.35, 178.62, 177.90, 177.19, 176.50, 175.82,
    175.15, 174.48, 173.83, 173.19, 172.56, 171.94, 171.33, 170.72, 170.12, 169.54, 168.95, 168.38,
    167.82, 167.26, 166.71, 166.16, 165.63, 165.10, 164.58, 164.06, 163.55, 163.03, 162.54, 162.05,
    161.57, 161.09, 160.61, 160.14, 159.67, 159.21, 158.75, 158.31, 157.86, 157.43, 157.00, 156.56,
    156.13, 155.70, 155.28, 154.86, 154.45, 154.04, 153.64, 153.24, 152.85, 152.46, 152.07, 151.69,
    151.31, 150.93, 150.54, 150.16, 149.79, 149.43, 149.07, 148.71, 148.36, 148.01, 147.66, 147.31,
    146.97, 146.63, 146.29, 145.95, 145.61, 145.28, 144.95, 144.62, 144.29, 143.97, 143.65, 143.34,
    143.03, 142.72, 142.41, 142.10, 141.79, 141.49, 141.19, 140.88, 140.58, 140.28, 139.99, 139.70,
    139.41, 139.12, 138.83, 138.55, 138.28, 138.00, 137.72, 137.44, 137.17, 136.89, 136.62, 136.35,
    136.07, 135.81, 135.54, 135.28, 135.01, 134.75, 134.49, 134.24, 133.98, 133.73, 133.48, 133.23,
    132.98, 132.73, 132.48, 132.23, 131.98, 131.74, 131.49, 131.25, 131.01, 130.77, 130.54, 130.30,
    130.07, 129.84, 129.61, 129.37, 129.14, 128.91, 128.68, 128.45, 128.23, 128.00, 127.77, 127.55,
    127.33, 127.10, 126.88, 126.67, 126.45, 126.23, 126.02, 125.80, 125.59, 125.38, 125.17, 124.95,
    124.74, 124.53, 124.32, 124.11, 123.91, 123.70, 123.50, 123.29, 123.09, 122.89, 122.68, 122.48,
    122.28, 122.08, 121.88, 121.69, 121.49, 121.30, 121.11, 120.91, 120.72, 120.53, 120.33, 120.14,
    119.95, 119.76, 119.57, 119.38, 119.19, 119.00, 118.81, 118.63, 118.44, 118.26, 118.07, 117.89,
    117.71, 117.53, 117.35, 117.17, 116.99, 116.81, 116.63, 116.45, 116.27, 116.09, 115.92, 115.74,
    115.57, 115.39, 115.22, 115.05, 114.87, 114.70, 114.53, 114.36, 114.19, 114.02, 113.85, 113.68,
    113.51, 113.34, 113.17, 113.01, 112.84, 112.67, 112.51, 112.34, 112.18, 112.01, 111.85, 111.68,
    111.52, 111.36, 111.20, 111.04, 110.88, 110.72, 110.56, 110.40, 110.24, 110.08, 109.93, 109.77,
    109.61, 109.45, 109.29, 109.14, 108.98, 108.82, 108.67, 108.52, 108.36, 108.21, 108.06, 107.91,
    107.75, 107.60, 107.45, 107.29, 107.14, 106.99, 106.84, 106.69, 106.54, 106.39, 106.25, 106.10,
    105.95, 105.80, 105.66, 105.51, 105.36, 105.22, 105.07, 104.92, 104.78, 104.64, 104.49, 104.35,
    104.20, 104.06, 103.91, 103.77, 103.63, 103.49, 103.34, 103.20, 103.06, 102.92, 102.78, 102.64,
    102.50, 102.36, 102.22, 102.08, 101.94, 101.80, 101.66, 101.52, 101.39, 101.25, 101.11, 100.97,
    100.84, 100.70, 100.57, 100.43, 100.30, 100.16, 100.03, 99.89, 99.76, 99.62, 99.49, 99.35,
    99.22, 99.08, 98.95, 98.82, 98.68, 98.55, 98.42, 98.29, 98.16, 98.02, 97.89, 97.76, 97.63,
    97.50, 97.37, 97.24, 97.11, 96.98, 96.85, 96.72, 96.59, 96.46, 96.33, 96.20, 96.07, 95.94,
    95.82, 95.69, 95.56, 95.43, 95.31, 95.18, 95.05, 94.93, 94.80, 94.67, 94.55, 94.42, 94.29,
    94.17, 94.04, 93.92, 93.79, 93.67, 93.54, 93.42, 93.29, 93.17, 93.05, 92.92, 92.80, 92.68,
    92.55, 92.43, 92.31, 92.19, 92.06, 91.94, 91.82, 91.70, 91.58, 91.45, 91.33, 91.21, 91.09,
    90.97, 90.85, 90.73, 90.61, 90.49, 90.37, 90.24, 90.12, 90.00, 89.88, 89.76, 89.64, 89.52,
    89.40, 89.29, 89.17, 89.05, 88.93, 88.81, 88.69, 88.57, 88.45, 88.34, 88.22, 88.10, 87.98,
    87.86, 87.75, 87.63, 87.51, 87.39, 87.28, 87.16, 87.04, 86.92, 86.81, 86.69, 86.57, 86.46,
    86.34, 86.22, 86.11, 85.99, 85.87, 85.76, 85.64, 85.53, 85.41, 85.30, 85.18, 85.07, 84.95,
    84.84, 84.72, 84.61, 84.49, 84.38, 84.26, 84.15, 84.03, 83.92, 83.80, 83.69, 83.57, 83.46,
    83.34, 83.23, 83.11, 83.00, 82.89, 82.78, 82.66, 82.55, 82.44, 82.33, 82.21, 82.10, 81.99,
    81.88, 81.76, 81.65, 81.53, 81.42, 81.30, 81.19, 81.07, 80.96, 80.85, 80.74, 80.62, 80.51,
    80.40, 80.29, 80.18, 80.06, 79.95, 79.84, 79.72, 79.61, 79.50, 79.38, 79.27, 79.16, 79.04,
    78.93, 78.82, 78.71, 78.60, 78.49, 78.37, 78.26, 78.15, 78.04, 77.93, 77.81, 77.70, 77.59,
    77.47, 77.36, 77.25, 77.14, 77.02, 76.91, 76.80, 76.69, 76.57, 76.46, 76.35, 76.24, 76.13,
    76.01, 75.91, 75.80, 75.69, 75.58, 75.47, 75.36, 75.25, 75.14, 75.03, 74.92, 74.81, 74.70,
    74.58, 74.47, 74.35, 74.24, 74.13, 74.01, 73.90, 73.79, 73.68, 73.57, 73.46, 73.35, 73.24,
    73.13, 73.02, 72.91, 72.79, 72.68, 72.57, 72.46, 72.35, 72.24, 72.13, 72.02, 71.91, 71.80,
    71.69, 71.58, 71.47, 71.36, 71.25, 71.14, 71.03, 70.92, 70.81, 70.69, 70.58, 70.47, 70.36,
    70.25, 70.13, 70.02, 69.91, 69.80, 69.69, 69.57, 69.46, 69.35, 69.24, 69.13, 69.02, 68.90,
    68.79, 68.68, 68.57, 68.45, 68.34, 68.23, 68.12, 68.00, 67.89, 67.78, 67.67, 67.55, 67.44,
    67.33, 67.22, 67.10, 66.99, 66.88, 66.76, 66.65, 66.54, 66.42, 66.31, 66.19, 66.08, 65.96,
    65.85, 65.74, 65.62, 65.51, 65.39, 65.28, 65.16, 65.05, 64.93, 64.82, 64.71, 64.59, 64.48,
    64.37, 64.25, 64.14, 64.03, 63.91, 63.80, 63.68, 63.57, 63.45, 63.34, 63.23, 63.11, 62.99,
    62.88, 62.76, 62.65, 62.53, 62.42, 62.30, 62.18, 62.07, 61.95, 61.83, 61.72, 61.60, 61.48,
    61.36, 61.25, 61.13, 61.01, 60.89, 60.78, 60.66, 60.54, 60.42, 60.30, 60.18, 60.07, 59.95,
    59.83, 59.71, 59.59, 59.47, 59.35, 59.23, 59.12, 59.00, 58.88, 58.76, 58.64, 58.52, 58.40,
    58.28, 58.16, 58.03, 57.91, 57.79, 57.67, 57.55, 57.43, 57.31, 57.18, 57.06, 56.94, 56.81,
    56.69, 56.57, 56.44, 56.32, 56.20, 56.07, 55.95, 55.82, 55.70, 55.57, 55.45, 55.32, 55.20,
    55.07, 54.95, 54.82, 54.70, 54.57, 54.44, 54.32, 54.19, 54.06, 53.94, 53.81, 53.68, 53.56,
    53.43, 53.30, 53.17, 53.05, 52.92, 52.79, 52.66, 52.53, 52.40, 52.27, 52.13, 52.00, 51.87,
    51.74, 51.61, 51.48, 51.35, 51.22, 51.08, 50.95, 50.82, 50.69, 50.55, 50.42, 50.29, 50.15,
    50.02, 49.88, 49.75, 49.61, 49.47, 49.34, 49.20, 49.06, 48.92, 48.79, 48.65, 48.51, 48.37,
    48.23, 48.09, 47.95, 47.81, 47.67, 47.53, 47.39, 47.24, 47.10, 46.96, 46.82, 46.67, 46.53,
    46.39, 46.24, 46.10, 45.95, 45.81, 45.66, 45.52, 45.37, 45.23, 45.08, 44.93, 44.78, 44.63,
    44.48, 44.33, 44.18, 44.03, 43.88, 43.73, 43.58, 43.42, 43.27, 43.12, 42.96, 42.81, 42.66,
    42.50, 42.34, 42.19, 42.03, 41.87, 41.72, 41.56, 41.40, 41.24, 41.08, 40.92, 40.76, 40.60,
    40.43, 40.27, 40.11, 39.94, 39.78, 39.61, 39.44, 39.27, 39.10, 38.93, 38.76, 38.59, 38.42,
    38.25, 38.08, 37.90, 37.73, 37.56, 37.38, 37.20, 37.02, 36.85, 36.67, 36.49, 36.31, 36.13,
    35.95, 35.76, 35.58, 35.40, 35.21, 35.02, 34.84, 34.65, 34.46, 34.27, 34.08, 33.88, 33.69,
    33.50, 33.30, 33.11, 32.91, 32.71, 32.51, 32.31, 32.11, 31.91, 31.70, 31.50, 31.29, 31.09,
    30.88, 30.67, 30.46, 30.25, 30.03, 29.82, 29.60, 29.38, 29.16, 28.94, 28.71, 28.49, 28.26,
    28.04, 27.81, 27.58, 27.34, 27.11, 26.87, 26.64, 26.40, 26.16, 25.91, 25.67, 25.42, 25.17,
    24.92, 24.67, 24.41, 24.15, 23.89, 23.63, 23.36, 23.10, 22.83, 22.56, 22.29, 22.01, 21.73,
    21.45, 21.16, 20.87, 20.58, 20.29, 19.99, 19.69, 19.39, 19.08, 18.77, 18.45, 18.13, 17.81,
    17.48, 17.15, 16.82, 16.49, 16.15, 15.81, 15.45, 15.10, 14.74, 14.37, 14.00, 13.63, 13.25,
    12.86, 12.47, 12.07, 11.67, 11.26, 10.84, 10.42, 9.98, 9.54, 9.09, 8.64, 8.17, 7.69, 7.20,
    6.71, 6.20, 5.69, 5.16, 4.62, 4.07, 3.50, 2.92, 2.33, 1.72, 1.09, 0.44, -0.22, -0.91, -1.63,
    -2.36, -3.13, -3.91, -4.73, -5.59, -6.47, -7.40, -8.37, -9.38, -10.46, -11.60, -12.81, -14.10,
    -15.48, -16.97, -18.57, -20.34, -22.30, -24.50, -27.00, -29.92, -33.49, -38.01, -44.37, -50.00,
    -50.00,
];

fn get_temp_from_adc(raw_adc: f32) -> f32 {
    let mut raw_val = raw_adc;
    raw_val = raw_val.clamp(0.0, 4095.0);

    let index_f = raw_val / 4.0;
    let index = index_f as usize;

    if index >= 1024 {
        return NTC_LUT[1024];
    }

    let remainder = index_f - (index as f32);
    let lower = NTC_LUT[index];
    let upper = NTC_LUT[index + 1];
    lower + (upper - lower) * remainder
}

const POWER_TO_DELAY_LUT: [f32; 101] = [
    0.6000, 0.5964, 0.5929, 0.5894, 0.5859, 0.5825, 0.5790, 0.5756, 0.5722, 0.5688, 0.5654, 0.5621,
    0.5587, 0.5554, 0.5521, 0.5488, 0.5455, 0.5422, 0.5389, 0.5357, 0.5324, 0.5291, 0.5259, 0.5226,
    0.5194, 0.5162, 0.5129, 0.5097, 0.5065, 0.5033, 0.5000, 0.4968, 0.4936, 0.4904, 0.4871, 0.4839,
    0.4807, 0.4774, 0.4742, 0.4709, 0.4677, 0.4644, 0.4612, 0.4579, 0.4546, 0.4513, 0.4480, 0.4447,
    0.4413, 0.4380, 0.4346, 0.4313, 0.4279, 0.4245, 0.4210, 0.4176, 0.4141, 0.4107, 0.4072, 0.4036,
    0.4001, 0.3965, 0.3929, 0.3893, 0.3856, 0.3819, 0.3782, 0.3744, 0.3706, 0.3668, 0.3629, 0.3590,
    0.3550, 0.3510, 0.3469, 0.3427, 0.3386, 0.3343, 0.3300, 0.3256, 0.3211, 0.3166, 0.3120, 0.3072,
    0.3024, 0.2975, 0.2924, 0.2873, 0.2820, 0.2765, 0.2709, 0.2651, 0.2591, 0.2529, 0.2464, 0.2397,
    0.2326, 0.2252, 0.2174, 0.2090, 0.2000,
];

fn get_delay_fraction(power_percent: f32) -> f32 {
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

pub fn setup_trigger_sm(
    common: &mut Common<'static, PIO0>,
    sm: &mut StateMachine<'static, PIO0, 1>,
    zc_pin: &Pin<'static, PIO0>,
) {
    let prg = pio_asm!(
        ".wrap_target",
        "wait 1 pin 0", // Wait for pin to go high
        "wait 0 pin 0", // Wait for pin to go low (detect falling edge of zero-cross)
        "mov x, !null", // Initialize X counter to 0xFFFFFFFF
        "low_loop:",
        "jmp pin, rising_edge", // If pin goes high, we found the next edge
        "jmp x--, low_loop",    // Decrement X and loop
        "rising_edge:",
        "mov isr, !x",  // ISR = NOT(X) = elapsed cycles
        "push noblock", // Push the period measurement to the RX FIFO
        ".wrap"
    );
    let loaded = common.load_program(&prg.program);
    let mut cfg = Config::default();
    cfg.use_program(&loaded, &[]);
    cfg.set_in_pins(&[zc_pin]);
    cfg.set_jmp_pin(zc_pin);
    cfg.clock_divider = FixedU32::from_num(150_000_000.0 / 2_000_000.0);
    sm.set_config(&cfg);
    sm.set_pin_dirs(Direction::In, &[zc_pin]);
    sm.set_enable(true);
}

pub fn setup_triac_sm(
    common: &mut Common<'static, PIO0>,
    sm: &mut StateMachine<'static, PIO0, 2>,
    triac_pin: &Pin<'static, PIO0>,
    zc_pin: &Pin<'static, PIO0>,
) {
    let prg = pio_asm!(
        ".wrap_target",
        "pull block",   // Pull phase delay from TX FIFO (block if empty)
        "mov x, osr",   // Move delay value to X counter
        "wait 1 pin 0", // Wait for Zero-Cross signal high
        "wait 0 pin 0", // Wait for Zero-Cross signal low (start of half-wave)
        "lp:",
        "jmp x-- lp",       // Wait for 'X' microseconds
        "set pins, 1 [30]", // Trigger Triac (pulse high for ~30 cycles)
        "set pins, 0",      // Set Triac gate low
        ".wrap"
    );
    let loaded = common.load_program(&prg.program);
    let mut cfg = Config::default();
    cfg.use_program(&loaded, &[]);
    cfg.set_set_pins(&[triac_pin]);
    cfg.set_out_pins(&[triac_pin]);
    cfg.set_in_pins(&[zc_pin]);
    cfg.clock_divider = FixedU32::from_num(150_000_000.0 / 1_000_000.0);
    sm.set_config(&cfg);
    sm.set_pin_dirs(Direction::Out, &[triac_pin]);
    sm.set_enable(true);
}

/// Heater PIO SM: zero-cross-synced ON/OFF with a hardware fail-safe.
///
/// `ac_sync_control_task` decides "fire/skip this chunk" (delta-sigma) but
/// never touches the heater pin directly — it just pushes `1` to this SM's
/// TX FIFO when the chunk should fire, or nothing when it shouldn't. The SM
/// checks its FIFO **once per full AC cycle** (a "chunk" = the sensed
/// positive half-wave + the following negative half-wave).
///
/// Two consecutive queued flags therefore hold the pin on for 2 full cycles
/// (4 half-waves) with no glitch in between (the SM just re-asserts `set
/// pins,1` at the next boundary). If `ac_sync_control_task` hangs or panics
/// and stops pushing, the FIFO drains and the very next boundary check
/// forces the pin low — the heater fails safe within at most one chunk
/// (~20ms @ 50Hz), entirely independent of the CPU/executor.
///
/// **Zero-cross race note:** GP10 is tapped after the step-down transformer
/// (see `sch/README.md`), so the sensed edge lags the true mains zero-cross.
/// Deciding right at `wait 0 pin 0` would race the MOC3062M's own true-zero-cross
/// retrigger sample: a late OFF flag could arrive just after the opto already
/// latched "conduct through this half-wave", silently stretching one chunk to
/// 3 half-waves instead of 2. To avoid that, the SM burns a fixed ~2ms settle
/// delay right after the sensed edge (a single `nop [31]`, using the SM's own
/// slow clock divider so its 5-bit delay field spans milliseconds) before it
/// ever samples the FIFO or touches the pin — landing the decision safely
/// mid-half-wave on both the sensed and true timelines, well clear of any
/// zero-cross window, using a hardware delay slot instead of a software timer
/// so the safety margin holds even if the task controlling it has died.
pub fn setup_heater_sm(
    common: &mut Common<'static, PIO0>,
    sm: &mut StateMachine<'static, PIO0, 0>,
    heater_pin: &Pin<'static, PIO0>,
    zc_pin: &Pin<'static, PIO0>,
) {
    let prg = pio_asm!(
        ".wrap_target",
        "wait 0 pin 0", // Sync: sensed start of positive half-wave (chunk boundary)
        "nop [31]",     // Fixed settle delay: 32 cycles @ 16kHz = exactly 2ms, clears
                        // the true-zero-cross race window regardless of transformer lag
        "set x, 0",     // Sentinel: 0 = "nothing queued this chunk"
        "pull noblock", // FIFO has data -> OSR=data; empty -> OSR=x (0)
        "mov x, osr",
        "jmp !x, off",
        "set pins, 1", // Flag present -> heater ON for the whole chunk
        "jmp cont",
        "off:",
        "set pins, 0", // FIFO empty -> heater OFF for the whole chunk (fail-safe)
        "cont:",
        "wait 1 pin 0", // Ride out the rest of the positive half-wave
        ".wrap"         // Loop back to "wait 0 pin 0": rides out the negative half-wave too
    );
    let loaded = common.load_program(&prg.program);
    let mut cfg = Config::default();
    cfg.use_program(&loaded, &[]);
    cfg.set_set_pins(&[heater_pin]);
    cfg.set_in_pins(&[zc_pin]);
    // 16kHz SM clock: only governs the settle-delay loop's granularity and the
    // "wait pin" instructions' edge-detection latency (both ms-scale-tolerant
    // here) — independent of the trigger/triac SMs' own clock dividers.
    cfg.clock_divider = FixedU32::from_num(150_000_000.0 / 16_000.0);
    sm.set_config(&cfg);
    sm.set_pin_dirs(Direction::Out, &[heater_pin]);
    sm.set_enable(true);
}

// Samples `total` conversions from `ch`, discarding the leading `total - keep`
// (letting the sample-and-hold cap settle) and returning the average of the rest.
async fn sample_avg(
    adc: &mut Adc<'static, Async>,
    ch: &mut Channel<'static>,
    total: usize,
    keep: usize,
) -> f32 {
    let discard = total - keep;
    let mut sum: u32 = 0;
    for i in 0..total {
        let v = adc.read(ch).await.unwrap_or(0) as u32;
        if i >= discard {
            sum += v;
        }
    }
    sum as f32 / keep as f32
}

#[embassy_executor::task]
pub async fn adc_task(
    mut adc: Adc<'static, Async>,
    mut ch_p: Channel<'static>,
    mut ch_t: Channel<'static>,
) {
    let (mut p_ema, mut t_ema) = (0.0f32, 0.0f32);
    let mut initialized = false;

    let mut ticker = embassy_time::Ticker::every(Duration::from_hz(500));

    loop {
        // Sample each channel `total` times; discard the first `total - keep` to allow the ADC
        // sample-and-hold capacitor to fully charge through the 1k series resistor on the analog
        // lines, then average the last `keep` samples to knock down noise before the EMA filter.
        let raw_p = sample_avg(&mut adc, &mut ch_p, 10, 5).await;
        let raw_t = sample_avg(&mut adc, &mut ch_t, 10, 5).await;

        if !initialized {
            p_ema = raw_p;
            t_ema = raw_t;
            initialized = true;
        } else {
            const ALPHA_P: f32 = 0.01; // ~0.8 Hz cutoff (rejects ~200ms beat from unsynced ADC/pump sampling)
            const ALPHA_T: f32 = 0.2; // ~20.0 Hz Cutoff
            p_ema = p_ema + ALPHA_P * (raw_p - p_ema);
            t_ema = t_ema + ALPHA_T * (raw_t - t_ema);
        }

        // Convert raw filtered ADC to physical units (0.4V-2.4V = 0-1.2 MPa)
        let v_p = p_ema * (3.3 / 4095.0);
        let p_bar = ((v_p - 0.4) * (12.0 / 2.0)).max(0.0);
        // Get raw physical boiler temperature
        let t_c = get_temp_from_adc(t_ema);

        // Fetch current state, update it, and broadcast
        let mut state = ADC_WATCH.try_get().unwrap_or_default();
        state.pressure_bar = p_bar;
        state.temp_c = t_c;
        ADC_WATCH.sender().send(state);

        ticker.next().await;
    }
}

#[embassy_executor::task]
pub async fn ac_sync_control_task(
    mut sm_trigger: StateMachine<'static, PIO0, 1>,
    mut sm_pump: StateMachine<'static, PIO0, 2>,
    mut sm_heater: StateMachine<'static, PIO0, 0>,
) {
    // EMA filter for AC period
    let mut ac_ema = 10_000.0;

    // Load initial settings
    let initial_s = Settings::get().await;
    let mut press_pid = PidController::new(
        initial_s.press_pid.kp,
        initial_s.press_pid.ki,
        initial_s.press_pid.kd,
    );

    let mut temp_pid = PidController::new(
        initial_s.temp_pid.kp,
        initial_s.temp_pid.ki,
        initial_s.temp_pid.kd,
    );

    // Dynamic targets
    let (mut target_p, mut flow_limit) = (0.0, 0.0);
    let mut direct_pump: Option<f32> = None;
    let mut target_t = initial_s.machine.brew_temp;
    let mut feed_forward: f32 = 0.0;
    let mut brew_active = false;

    let mut tick: u32 = 0;
    let mut heater_duty = 0.0;
    let mut heater_accumulator = 0.0;

    loop {
        // --- Zero-Cross Detection & AC Frequency Tracking ---
        // Wait for a pulse from the PIO trigger SM (which measures half-wave period in microseconds)
        // We use a very short timeout here to keep the loop spinning even if AC is not connected.
        let zc_res =
            embassy_time::with_timeout(Duration::from_millis(25), sm_trigger.rx().wait_pull())
                .await;

        if let Ok(mut period_us_raw) = zc_res {
            // Drain any buffered results to get the most recent one
            while let Some(latest) = sm_trigger.rx().try_pull() {
                period_us_raw = latest;
            }
            let half_wave_us = period_us_raw as f32;
            // Validate period (should be ~10,000us for 50Hz or ~8,333us for 60Hz)
            if half_wave_us > 7_500.0 && half_wave_us < 11_500.0 {
                const ALPHA_AC: f32 = 0.10;
                ac_ema = ac_ema + ALPHA_AC * (half_wave_us - ac_ema);
            }
        }

        // --- Sensor Data Retrieval (From ADC Watch) ---
        let state = ADC_WATCH.try_get().unwrap_or_default();
        let p_ema = state.pressure_bar;
        let t_ema = state.temp_c;

        let s = crate::settings::ControlSettings::current();

        // --- Command & Signal Processing ---
        if let Some(tp) = SIG_TARGET_PRESSURE.try_take() {
            press_pid.set_coeffs(s.press_pid.kp, s.press_pid.ki, s.press_pid.kd);
            if target_p == 0.0 && tp != 0.0 {
                press_pid.reset();
            }
            target_p = tp;
        }
        if let Some(fl) = SIG_FLOW_LIMIT.try_take() {
            flow_limit = fl;
        }
        if let Some(dp) = SIG_DIRECT_PUMP.try_take() {
            direct_pump = dp;
        }
        if let Some(ba) = SIG_BREW_ACTIVE.try_take() {
            brew_active = ba;
        }
        if let Some(tt) = SIG_TARGET_TEMP.try_take() {
            temp_pid.set_coeffs(s.temp_pid.kp, s.temp_pid.ki, s.temp_pid.kd);
            if target_t == 0.0 && tt != 0.0 {
                temp_pid.reset();
            }
            target_t = tt;
            const CONST_FF: f32 = 0.021; // balance for the boiler/brew group
            feed_forward = CONST_FF * (s.machine.feed_forward_percents / 100.0) * (target_t - 20.0);
        }

        // --- Global Telemetry Update ---
        let mut new_state = state;
        new_state.target_bar = target_p;
        new_state.flow_limit_ml_s = flow_limit;
        new_state.target_temp = target_t;
        new_state.heater_duty = heater_duty;
        ADC_WATCH.sender().send(new_state);

        // --- Get flow readings ---
        let f = crate::flow_meter::FlowMonitor::new().get_state().await;

        // --- Pump Control (Triac Phase Angle) ---
        let p_output: f32 = match direct_pump {
            // Direct-pump mode (hot water/cooldown-flush/descale) needs no flow
            // limiting — it's raw power, not an espresso shot being protected.
            Some(dp) => dp.clamp(0.0, 100.0),
            None if target_p > 0.0 => {
                // Proportional flow-limit backoff, recomputed fresh each tick
                // (no accumulator). MARGIN gives some safety headroom below
                // flow_limit before flow settles at equilibrium.
                const MARGIN: f32 = 1.2;
                let effective_target_p = if flow_limit > 0.0 {
                    let flow_error = f.flow_rate_ml_s * MARGIN - flow_limit;
                    (target_p - s.machine.flow_limit_kp * flow_error).clamp(0.2, target_p)
                } else {
                    target_p
                };
                press_pid.update(effective_target_p, p_ema)
            }
            None => 0.0,
        };

        // If output is set, push the phase delay to the Triac PIO
        if p_output > 0.0 {
            let delay = get_delay_fraction(p_output) * ac_ema;
            sm_pump.tx().push(delay as u32);
        }

        // --- TEMPERATURE PID (Runs 5 times a second -> every 10 ticks at 50Hz) ---
        if tick.is_multiple_of(10) {
            let effective_target_t = if brew_active {
                target_t + (f.flow_rate_ml_s * feed_forward).clamp(0.0, 20.0)
            } else {
                target_t
            };
            heater_duty = temp_pid.update(effective_target_t, t_ema);
        }

        // --- HEATER PIO FLAG (Delta-Sigma decision) ---
        // The heater SM (setup_heater_sm) owns the actual zero-cross-synced pin
        // toggle and its own fixed settle delay — we just tell it "fire this
        // chunk" or not. Pushing nothing on a "skip" cycle is deliberate: the
        // SM's non-blocking FIFO check already defaults to OFF, and that same
        // default is what makes the heater fail safe if this task ever hangs.
        heater_accumulator += heater_duty;
        if heater_accumulator >= 100.0 {
            heater_accumulator -= 100.0;
            sm_heater.tx().push(1); // Flag: fire heater for this chunk
        }

        tick = tick.wrapping_add(1);
    }
}

pub async fn execute_profile(profile: BrewProfile) {
    defmt::info!("Executing profile: {}", profile.name.as_str());
    let mut pump = PumpGuard::engage(PumpMode::Pressure(0.0));
    let _brew_active = BrewActiveGuard::engage();

    // Yield once so the flow task can process the SIG_RESET_VOLUME signal that
    // transition_state() sent before dispatching this command. get_state() is
    // async but never suspends (it calls try_get() synchronously), so without
    // this yield the flow task never runs and vol_fut would see the stale
    // shot_volume_ml from the previous profile on every other run.
    yield_now().await;

    for (i, step) in profile.steps.iter().enumerate() {
        let mut time_s = step.time_s.unwrap_or(120.0);
        let volume = step.volume.unwrap_or(0.0);
        let pressure = step.pressure.unwrap_or(0.0);
        let flow = step.flow.unwrap_or(0.0);

        if time_s == 0.0 && volume == 0.0 {
            continue;
        }

        if time_s == 0.0 {
            time_s = 120.0;
        }

        defmt::info!(
            "Step {}: P={}bar, F={}ml/s, T={}s, V={}ml",
            i,
            pressure,
            flow,
            time_s,
            volume
        );

        set_flow_limit(flow);
        if pressure > 10.0 {
            pump.set_mode(PumpMode::DirectPump(pressure));
        } else {
            pump.set_mode(PumpMode::Pressure(pressure));
        }

        let time_fut = async {
            if time_s > 0.0 {
                Timer::after(Duration::from_millis((time_s * 1000.0) as u64)).await;
                defmt::info!("Step {} time limit reached", i);
            } else {
                core::future::pending::<()>().await;
            }
        };
        let vol_fut = async {
            if volume > 0.0 {
                loop {
                    // volume is cumulative across the whole profile (volume_ml
                    // is reset to 0 at profile start by transition_state).
                    if crate::flow_meter::FlowMonitor::new()
                        .get_state()
                        .await
                        .volume_ml
                        >= volume
                    {
                        defmt::info!("Step {} volume limit reached", i);
                        break;
                    }
                    Timer::after(Duration::from_millis(50)).await;
                }
            } else {
                core::future::pending::<()>().await;
            }
        };
        let res = select(
            select(time_fut, vol_fut),
            Timer::after(Duration::from_secs(120)),
        )
        .await;

        if let Either::Second(_) = res {
            defmt::warn!("Step {} hit safety timeout (120s)!", i);
        }
    }
    defmt::info!("Profile '{}' completed\r\n", profile.name.as_str());
    // `pump` drops here (or at the cancellation point if aborted), resetting
    // direct pump and the pressure target back to idle.
}

pub async fn execute_steam() {
    let s = Settings::get().await;
    set_target_temp(TargetTempMode::Steam).await;
    Timer::after(Duration::from_secs(s.machine.steam_time_limit_s as u64)).await;
    set_target_temp(TargetTempMode::Brew).await;
}

pub async fn execute_cooldown_flush() {
    let s = Settings::get().await;
    // Drop the target to 0 (heater off) instead of brew temp — the heater
    // fighting the incoming cold water only slows the cooldown down.
    set_target_temp(TargetTempMode::Off).await;
    let _pump = PumpGuard::engage(PumpMode::DirectPump(PUMP_POWER));

    let monitor = AdcMonitor::new();
    loop {
        let t_c = monitor.get_state().await.temp_c;
        if t_c <= s.machine.brew_temp + s.machine.temp_offset {
            break;
        }
        Timer::after(Duration::from_millis(100)).await;
    }
}

pub async fn execute_descale() {
    const DESCALE_VOLUME_ML: f32 = 200.0;
    const DESCALE_SOAK_SECS: u64 = 2 * 60;
    const FLOW_START_GRACE_SECS: u64 = 1;
    const FLOW_STALL_THRESHOLD_ML_S: f32 = 0.5;

    set_target_temp(TargetTempMode::Descale).await;

    loop {
        // --- Pump 200 ml ---
        let flow = crate::flow_meter::FlowMonitor::new();
        flow.reset_volume();
        let tank_empty = {
            let _pump = PumpGuard::engage(PumpMode::DirectPump(PUMP_POWER));

            // Allow time for flow to establish before checking for an empty tank.
            Timer::after(Duration::from_secs(FLOW_START_GRACE_SECS)).await;

            loop {
                Timer::after(Duration::from_millis(200)).await;
                let state = flow.get_state().await;

                if state.flow_rate_ml_s < FLOW_STALL_THRESHOLD_ML_S {
                    // Flow stopped while pump is running — tank is empty.
                    defmt::info!("Descale: no flow detected, tank empty.");
                    break true;
                }
                if state.volume_ml >= DESCALE_VOLUME_ML {
                    defmt::info!("Descale: {}ml dispensed.", DESCALE_VOLUME_ML);
                    break false;
                }
            }
            // `_pump` drops here, returning the pump to idle before the soak.
        };

        if tank_empty {
            break;
        }

        // --- Soak ---
        defmt::info!("Descale: soaking for {} s...", DESCALE_SOAK_SECS);
        Timer::after(Duration::from_secs(DESCALE_SOAK_SECS)).await;
    }

    set_target_temp(TargetTempMode::Brew).await;
}

pub async fn execute_direct_pump(power: f32) {
    let _pump = PumpGuard::engage(PumpMode::DirectPump(power));
    core::future::pending::<()>().await;
}

// ==========================================
// HARDWARE EXECUTOR TASK
// ==========================================

/// Whether a hardware operation needs the solenoid valve open (pressurizing
/// the group head) while it runs. Operations that don't need it just leave
/// it alone — it's already closed by the invariant that `SolenoidGuard`
/// always closes it again on drop, and it starts closed at boot.
#[derive(Clone, Copy)]
enum Solenoid {
    Open,
    Closed,
}

/// RAII guard for the solenoid valve: opens it when created and
/// unconditionally closes it again when dropped. This is the Rust
/// equivalent of a context manager — every exit path (natural finish,
/// abort, or a future early return/panic) closes the valve without relying
/// on a manual `set_low()` at the end of the function. Only constructed for
/// operations that actually open the valve (see `Solenoid`).
struct SolenoidGuard<'a> {
    valve: &'a mut Output<'static>,
}

impl<'a> SolenoidGuard<'a> {
    fn open(valve: &'a mut Output<'static>) -> Self {
        valve.set_high();
        Self { valve }
    }
}

impl Drop for SolenoidGuard<'_> {
    fn drop(&mut self) {
        self.valve.set_low();
    }
}

/// Owns the single solenoid valve GPIO and drives cancellable hardware
/// operations through it. Since there is exactly one valve in the system,
/// bundling it here means call sites just say `executor.run_cancellable(...)`
/// instead of threading `&mut valve` through every call.
struct HardwareExecutor {
    valve: Output<'static>,
}

impl HardwareExecutor {
    fn new(valve: Output<'static>) -> Self {
        Self { valve }
    }

    async fn run_cancellable<F: core::future::Future>(
        &mut self,
        solenoid: Solenoid,
        action_name: &'static str,
        fut: F,
    ) {
        // Closed operations never touch the valve — it's already closed.
        let _solenoid =
            matches!(solenoid, Solenoid::Open).then(|| SolenoidGuard::open(&mut self.valve));
        let abort = core::pin::pin!(SIG_PROFILE_ABORT.wait());
        let run = core::pin::pin!(fut);
        match select(run, abort).await {
            Either::First(_) => {
                defmt::info!("Hardware: {} finished naturally", action_name);
                // A Stop (or a new transition) racing the last instant of this
                // operation may have signaled abort just as `run` won the race
                // above — discard it now so it can't spuriously cancel the
                // *next* operation (see coordinator::transition_state/stop_to_idle,
                // which only signal abort while a busy operation is in flight).
                SIG_PROFILE_ABORT.reset();
                SIG_COMMAND.signal(MachineCommand::ProfileFinished);
            }
            Either::Second(_) => {
                defmt::warn!("Hardware: {} aborted", action_name);
            }
        }
        // `_solenoid` drops here (if it was Open), closing the valve regardless
        // of which branch ran.
    }
}


#[embassy_executor::task]
pub async fn hardware_task(valve: Output<'static>) {
    let mut executor = HardwareExecutor::new(valve);
    loop {
        let cmd = SIG_HARDWARE_CMD.wait().await;
        defmt::info!("Hardware task received command");

        match cmd {
            HardwareCommand::RunProfile(p) => {
                defmt::info!("Hardware: Starting profile '{}'", p.name.as_str());
                executor
                    .run_cancellable(Solenoid::Open, "Profile", execute_profile(p))
                    .await;
            }
            HardwareCommand::Steam => {
                defmt::info!("Hardware: Starting steam");
                executor
                    .run_cancellable(Solenoid::Closed, "Steam", execute_steam())
                    .await;
            }
            HardwareCommand::Descale => {
                defmt::info!("Hardware: Starting descale");
                executor
                    .run_cancellable(Solenoid::Closed, "Descale", execute_descale())
                    .await;
            }
            HardwareCommand::CooldownFlush => {
                defmt::info!("Hardware: Starting cooldown flush");
                executor
                    .run_cancellable(Solenoid::Closed, "Cooldown flush", execute_cooldown_flush())
                    .await;
            }
            HardwareCommand::DirectPump(power) => {
                defmt::info!("Hardware: Starting direct pump {}%", power);
                executor
                    .run_cancellable(Solenoid::Open, "Direct pump", execute_direct_pump(power))
                    .await;
            }
            HardwareCommand::HotWater => {
                defmt::info!("Hardware: Starting hot water");
                executor
                    .run_cancellable(
                        Solenoid::Closed,
                        "Hot water",
                        execute_direct_pump(PUMP_POWER),
                    )
                    .await;
            }
        }
    }
}
