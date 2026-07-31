//! The real-time control loop and the shared vocabulary used to drive it.
//!
//! `ac_sync_control_task` is the single writer of the triac and heater FIFOs:
//! it tracks the mains period, runs the pressure and temperature PIDs, and
//! delta-sigma modulates the heater. Everything else in this module is the
//! surface other tasks use to steer that loop -- signals plus RAII guards that
//! guarantee the pump and brew flags are released on every exit path.
//!
//! The PIO0 programs live here too rather than in their own module: the FIFO
//! word formats they define (microsecond phase delay for the triac, a single
//! sentinel word per chunk for the heater) are only meaningful together with
//! the loop below that writes them, and nothing in the type system links the
//! two. This matches `leds` and `flow_meter`, which also keep each SM's setup
//! next to the task that drives it.

use embassy_rp::peripherals::PIO0;
use embassy_rp::pio::{Common, Config, Direction, Pin, StateMachine};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::Duration;
use fixed::FixedU32;
use pio::pio_asm;

use crate::pid::PidController;
use crate::settings::Settings;
use crate::state::TELEMETRY_WATCH;

pub static SIG_TARGET_TEMP: Signal<CriticalSectionRawMutex, f32> = Signal::new();

pub enum TargetTempMode {
    Brew,
    Steam,
    Off,
}

pub async fn set_target_temp(mode: TargetTempMode) {
    let s = crate::settings::ControlSettings::current();
    let temp = match mode {
        TargetTempMode::Brew => crate::state::get_session_brew_temp() + s.machine.temp_offset,
        TargetTempMode::Steam => s.machine.steam_temp,
        TargetTempMode::Off => 0.0,
    };
    SIG_TARGET_TEMP.signal(temp);
}

/// How the pump should be driven, sent as a single atomic message
#[derive(Clone, Copy, PartialEq)]
pub enum PumpMode {
    Idle,
    DirectPump(f32),
    Pressure { bar: f32, flow_limit_ml_s: f32 },
}

impl PumpMode {
    /// (bar, flow_limit_ml_s) for Pressure mode, or (0.0, 0.0) otherwise —
    /// lets the control loop treat "not in Pressure mode" the same as "no
    /// pressure target" without a separate match everywhere it's needed.
    fn pressure_and_flow_limit(&self) -> (f32, f32) {
        match *self {
            PumpMode::Pressure {
                bar,
                flow_limit_ml_s,
            } => (bar, flow_limit_ml_s),
            _ => (0.0, 0.0),
        }
    }

    /// Signals the mode directly, for callers outside a `PumpGuard`
    pub fn apply(self) {
        SIG_PUMP_MODE.signal(self);
    }
}

pub static SIG_PUMP_MODE: Signal<CriticalSectionRawMutex, PumpMode> = Signal::new();

// RAII guard for the pump: applies `mode` when created/changed, and always
// returns the pump to idle when dropped
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
        PumpMode::Idle.apply();
    }
}

pub static SIG_BREW_ACTIVE: Signal<CriticalSectionRawMutex, bool> = Signal::new();

/// RAII guard marking that a real brew profile (not cooldown flush, hot water,
/// or a raw direct-pump command) is running. `ac_sync_control_task` only
/// substitutes `target` for the real measurement (freezing the PID's output,
/// see the temp-control loop below) while this is armed — those other
/// operations run at the temperature their machine state implies (often `Off`,
/// to let cold water cool the boiler as fast as possible) and must keep
/// tracking it via the normal PID, not have the heater frozen just because the
/// pump is flowing.
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
    cfg.clock_divider = FixedU32::from_num(crate::board::SYS_CLK_HZ / 2_000_000.0);
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
    cfg.clock_divider = FixedU32::from_num(crate::board::SYS_CLK_HZ / 1_000_000.0);
    sm.set_config(&cfg);
    sm.set_pin_dirs(Direction::Out, &[triac_pin]);
    sm.set_enable(true);
}

/// Pump power percentage to the triac's firing delay, as a fraction of the
/// half-wave measured by the trigger SM. Non-linear because a phase-angle
/// dimmer's delivered power follows the integral of the sine, not the delay:
/// 0% fires at 0.6 of the half-wave, 100% at 0.2. Lives here rather than in a
/// calibration module because `setup_triac_sm` above defines the units of the
/// word this feeds, and `ac_sync_control_task` below is the only caller.
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

/// Heater PIO SM: zero-cross-synced ON/OFF with a hardware fail-safe.
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
    cfg.clock_divider = FixedU32::from_num(crate::board::SYS_CLK_HZ / 16_000.0);
    sm.set_config(&cfg);
    sm.set_pin_dirs(Direction::Out, &[heater_pin]);
    sm.set_enable(true);
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
    let mut mode = PumpMode::Idle;
    let mut effective_target_p: f32 = 0.0;
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
        let state = TELEMETRY_WATCH.try_get().unwrap_or_default();
        let p_ema = state.pressure_bar;
        let t_ema = state.temp_c;

        let s = crate::settings::ControlSettings::current();

        // --- Command & Signal Processing ---
        let (mut target_p, mut flow_limit) = mode.pressure_and_flow_limit();
        if let Some(new_mode) = SIG_PUMP_MODE.try_take() {
            let (new_bar, new_fl) = new_mode.pressure_and_flow_limit();
            press_pid.set_coeffs(s.press_pid.kp, s.press_pid.ki, s.press_pid.kd);
            press_pid.reset_if_reactivated(target_p, new_bar);
            // Reseed the flow-limit accumulator exactly when flow-limited
            // pressure control transitions inactive -> active
            if new_bar > 0.0 && new_fl > 0.0 && !(target_p > 0.0 && flow_limit > 0.0) {
                effective_target_p = p_ema;
            }
            mode = new_mode;
            target_p = new_bar;
            flow_limit = new_fl;
        }
        let direct_pump = match mode {
            PumpMode::DirectPump(power) => Some(power),
            _ => None,
        };
        if let Some(ba) = SIG_BREW_ACTIVE.try_take() {
            brew_active = ba;
        }
        if let Some(tt) = SIG_TARGET_TEMP.try_take() {
            temp_pid.set_coeffs(s.temp_pid.kp, s.temp_pid.ki, s.temp_pid.kd);
            temp_pid.reset_if_reactivated(target_t, tt);
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
        TELEMETRY_WATCH.sender().send(new_state);

        // --- Get flow readings ---
        let f = crate::flow_meter::get_flow();

        // --- Pump Control (Triac Phase Angle) ---
        let p_output: f32 = match direct_pump {
            // Direct-pump mode (hot water / cooldown flush / flush) needs no
            // flow limiting — it's raw power, not an espresso shot being
            // protected.
            Some(dp) => dp.clamp(0.0, 100.0),
            None if target_p > 0.0 => {
                // Accumulator-based flow-limit backoff
                if flow_limit > 0.0 {
                    let flow_error = f.flow_rate_ml_s - flow_limit;
                    effective_target_p -= s.machine.flow_limit_kp * flow_error;
                    effective_target_p = effective_target_p.clamp(0.2, target_p);
                } else {
                    effective_target_p = target_p;
                }
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
        heater_accumulator += heater_duty;
        if heater_accumulator >= 100.0 {
            heater_accumulator -= 100.0;
            sm_heater.tx().push(1); // Flag: fire heater for this chunk
        }

        tick = tick.wrapping_add(1);
    }
}
