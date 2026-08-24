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
use crate::state::{Telemetry, TELEMETRY_WATCH};

static SIG_TARGET_TEMP: Signal<CriticalSectionRawMutex, f32> = Signal::new();

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
    /// Both fields are ceilings, and whichever binds first governs the pump.
    /// A `bar` of 0.0 means "no pressure ceiling", which selects the plain flow
    /// loop; any non-zero `bar` selects the unified normalised controller.
    Pressure {
        bar: f32,
        flow_limit_ml_s: f32,
    },
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

static SIG_PUMP_MODE: Signal<CriticalSectionRawMutex, PumpMode> = Signal::new();

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

static SIG_BREW_ACTIVE: Signal<CriticalSectionRawMutex, bool> = Signal::new();

/// RAII guard marking that a real brew profile (not cooldown flush, hot water,
/// or a raw direct-pump command) is running. While it is armed,
/// `ac_sync_control_task` adds a flow-proportional feed-forward term to the
/// *temperature setpoint* (see the temp-control loop below): cold water
/// entering the boiler at `flow_rate_ml_s` drags the measured temperature down
/// faster than feedback alone can answer, so the target is raised in
/// proportion to the flow to compensate. The PID itself keeps running
/// normally; only its setpoint moves.
///
/// The other pumping operations deliberately do not get that boost. They run
/// at the temperature their machine state implies — often `Off`, to let cold
/// water cool the boiler as fast as possible — and raising their setpoint just
/// because the pump is flowing would fight the point of the operation.
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

/// Channel weights for the unified normalised pump controller. Each channel is
/// mapped to `1 + w * (measurement / ceiling - 1)`, so `w` sets how steeply it
/// approaches its ceiling without moving the fixed point, which stays at 1.0
/// for any `w`. Because the physical gain a channel sees is `k * w / ceiling`,
/// these also fix the split of the single shared gain set between the two:
/// `W_FLOW = 1.0` is the reference, and `W_PRESSURE` is chosen so a 9 bar
/// ceiling reproduces the 10 %/bar the pressure loop was tuned to when it
/// worked directly in bar. The larger weight also gives the ceiling the right
/// shape — the pressure channel stays dormant until within `1/W_PRESSURE` of
/// the target, then bites hard.
const W_PRESSURE: f32 = 7.5;
const W_FLOW: f32 = 1.0;

/// Pump power percentage to the triac's firing delay, as a fraction of the
/// half-wave measured by the trigger SM. Non-linear because a phase-angle
/// dimmer's delivered power follows the integral of the sine, not the delay:
/// the table is the inverse of `P(a)/P_full = [(pi - a) + sin(2a)/2] / pi`,
/// sampled so that each 1% step is an equal step in *delivered power*.
/// The endpoints set the usable pump range: 0% fires at 0.80 of the half-wave
/// (144 deg, 4.9% power), 100% at 0.20 (36 deg, 95.1% power), so
/// `P(x%) = 4.86% + x * 0.903%`.
const POWER_TO_DELAY_LUT: [f32; 101] = [
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
    // Drives the pump whenever a step names a pressure ceiling, from the
    // normalised measurement built in the duty match below.
    let mut pump_pid = PidController::new(&initial_s.press_pid);

    let mut temp_pid = PidController::new(&initial_s.temp_pid);

    // Drives pump duty straight from the flow error. Used for steps that name
    // no pressure ceiling, where the OPV is the pressure backstop rather than
    // the sensor.
    let mut flow_pid = PidController::new(&initial_s.flow_pid);

    // Dynamic targets
    let mut mode = PumpMode::Idle;
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
        let sensors = crate::state::get_sensors();
        let p_ema = sensors.pressure_bar;
        let t_ema = sensors.temp_c;

        let s = crate::settings::ControlSettings::current();

        // Read once and reused for the rest of the tick, so the control
        // decision and the telemetry row describe the same instant.
        let flow_ml_s = crate::flow_meter::flow_rate_ml_s();

        // --- Command & Signal Processing ---
        let (mut target_p, mut flow_limit) = mode.pressure_and_flow_limit();
        if let Some(new_mode) = SIG_PUMP_MODE.try_take() {
            let (new_bar, new_fl) = new_mode.pressure_and_flow_limit();
            pump_pid.set_coeffs(&s.press_pid);
            // The normalised setpoint is always 1.0, so `reset_if_reactivated`
            // can never see the 0 -> non-zero edge it looks for. Key the reset
            // off the pump itself instead, preserving "only the start of a shot
            // clears the integral".
            let was_active = target_p > 0.0 || flow_limit > 0.0;
            let now_active = new_bar > 0.0 || new_fl > 0.0;
            if !was_active && now_active {
                pump_pid.reset();
            }
            flow_pid.set_coeffs(&s.flow_pid);
            // Only the start of a shot clears the integral.
            flow_pid.reset_if_reactivated(flow_limit, new_fl);
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
            temp_pid.set_coeffs(&s.temp_pid);
            temp_pid.reset_if_reactivated(target_t, tt);
            target_t = tt;
            const CONST_FF: f32 = 0.021; // balance for the boiler/brew group
            feed_forward = CONST_FF * (s.machine.feed_forward_percents / 100.0) * (target_t - 20.0);
        }

        // --- Pump Control (Triac Phase Angle) ---
        // Setpoint the pressure channel actually chased, kept at 0 whenever the
        // pressure loop isn't running so telemetry never reports a stale
        // target left over from the previous shot.
        let mut active_target_p = 0.0;
        // Set by the duty match below: true while the flow channel is the one
        // holding the pump back.
        let mut flow_controlled = false;
        let p_output: f32 = match direct_pump {
            // Direct-pump mode (hot water / cooldown flush / flush) needs no
            // flow control — it's raw power, not an espresso shot.
            Some(dp) => dp.clamp(0.0, 100.0),
            // A pressure ceiling puts the unified normalised controller in
            // charge. Each channel is mapped onto a common dimensionless axis
            // where its own ceiling is exactly 1.0, and the larger value wins —
            // taking the max of the measurements yields the min of the duties,
            // so the tighter constraint governs. The mapping is affine rather
            // than a plain ratio so the fixed point stays at the ceiling for
            // any weight; `w * measurement / target` would settle at
            // `target / w` instead.
            None if target_p > 0.0 => {
                active_target_p = target_p;
                let u_p = 1.0 + W_PRESSURE * (p_ema / target_p - 1.0);
                // An absent flow ceiling must never win the max().
                let u_q = if flow_limit > 0.0 {
                    1.0 + W_FLOW * (flow_ml_s / flow_limit - 1.0)
                } else {
                    f32::NEG_INFINITY
                };
                flow_controlled = u_q >= u_p;
                pump_pid.update(1.0, u_q.max(u_p))
            }
            // No pressure ceiling: the flow error drives duty directly and
            // maximum pressure is whatever the OPV allows.
            None if flow_limit > 0.0 => {
                flow_controlled = true;
                flow_pid.update(flow_limit, flow_ml_s)
            }
            None => 0.0,
        };

        // If output is set, push the phase delay to the Triac PIO
        if p_output > 0.0 {
            let delay = get_delay_fraction(p_output) * ac_ema;
            sm_pump.tx().push(delay as u32);
        }

        // --- TEMPERATURE PID (Runs 5 times a second -> every 10 ticks at 50Hz) ---
        // The effective target is recomputed every tick even though the PID
        // consumes it every tenth, so telemetry reports the setpoint implied by
        // the current flow rather than one up to 200 ms stale.
        let effective_target_t = if brew_active {
            target_t + (flow_ml_s * feed_forward).clamp(0.0, 20.0)
        } else {
            target_t
        };
        if tick.is_multiple_of(10) {
            heater_duty = temp_pid.update(effective_target_t, t_ema);
        }

        // --- HEATER PIO FLAG (Delta-Sigma decision) ---
        heater_accumulator += heater_duty;
        if heater_accumulator >= 100.0 {
            heater_accumulator -= 100.0;
            sm_heater.tx().push(1); // Flag: fire heater for this chunk
        }

        // --- Global Telemetry Update ---
        // Published at the end of the tick
        TELEMETRY_WATCH.sender().send(Telemetry {
            tick,
            pressure_bar: p_ema,
            temp_c: t_ema,
            target_bar: target_p,
            effective_target_bar: active_target_p,
            flow_limit_ml_s: flow_limit,
            flow_rate_ml_s: flow_ml_s,
            volume_ml: crate::flow_meter::shot_volume_ml(),
            flow_controlled,
            target_temp: target_t,
            effective_target_temp: effective_target_t,
            heater_duty,
            pump_duty: p_output,
        });

        tick = tick.wrapping_add(1);
    }
}
