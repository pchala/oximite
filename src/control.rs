use core::pin::pin;
use embassy_futures::select::{select, Either};
use embassy_rp::adc::{Adc, Async, Channel};
use embassy_rp::gpio::Output;
use embassy_rp::peripherals::PIO0;
use embassy_rp::pio::{Common, Config, Direction, Pin, StateMachine};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_sync::watch::Watch;
use embassy_time::{Duration, Timer};
use fixed::FixedU32;
use pio::pio_asm;

use crate::settings::{BrewProfile, SettingsManager};

pub static SIG_TARGET_PRESSURE: Signal<CriticalSectionRawMutex, f32> = Signal::new();
pub static SIG_FLOW_LIMIT: Signal<CriticalSectionRawMutex, f32> = Signal::new();
pub static SIG_TARGET_TEMP: Signal<CriticalSectionRawMutex, f32> = Signal::new();
pub static SIG_PROFILE_ABORT: Signal<CriticalSectionRawMutex, ()> = Signal::new();

#[derive(Clone)]
pub enum HardwareCommand {
    RunProfile(BrewProfile),
    Steam,
    Descale,
}
pub static SIG_HARDWARE_CMD: Signal<CriticalSectionRawMutex, HardwareCommand> = Signal::new();

pub fn set_target_pressure(bar: f32) {
    SIG_TARGET_PRESSURE.signal(bar);
}
pub fn set_flow_limit(ml_s: f32) {
    SIG_FLOW_LIMIT.signal(ml_s);
}
pub fn set_target_temp(c: f32) {
    SIG_TARGET_TEMP.signal(c);
}

#[derive(Clone, Copy, Default)]
pub struct AdcState {
    pub pressure_bar: f32,
    pub temp_c: f32,
    pub target_bar: f32,
    pub target_temp: f32,
    pub flow_limit_ml_s: f32,
    pub half_wave_us: f32,
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
            half_wave_us: 10_000.0,
        })
    }
}

const POWER_TO_DELAY_LUT: [f32; 101] = [
    1.000, 0.915, 0.886, 0.864, 0.846, 0.830, 0.816, 0.803, 0.791, 0.780, 0.770, 0.760, 0.750,
    0.741, 0.732, 0.724, 0.716, 0.708, 0.700, 0.693, 0.686, 0.679, 0.672, 0.665, 0.658, 0.652,
    0.645, 0.639, 0.632, 0.626, 0.620, 0.614, 0.608, 0.602, 0.596, 0.590, 0.584, 0.578, 0.572,
    0.567, 0.561, 0.555, 0.550, 0.544, 0.539, 0.533, 0.528, 0.522, 0.517, 0.511, 0.506, 0.500,
    0.494, 0.489, 0.483, 0.478, 0.472, 0.467, 0.461, 0.456, 0.450, 0.445, 0.439, 0.433, 0.428,
    0.422, 0.416, 0.410, 0.404, 0.398, 0.392, 0.386, 0.380, 0.374, 0.368, 0.361, 0.355, 0.348,
    0.342, 0.335, 0.328, 0.321, 0.314, 0.307, 0.300, 0.292, 0.284, 0.276, 0.268, 0.259, 0.250,
    0.240, 0.230, 0.220, 0.209, 0.197, 0.184, 0.170, 0.154, 0.136, 0.000,
];

fn get_delay_fraction(power_percent: f32) -> f32 {
    let p = power_percent.clamp(0.0, 100.0);
    let index = p as usize;
    if index >= 100 {
        return 0.0;
    }
    let remainder = p - (index as f32);
    let lower = POWER_TO_DELAY_LUT[index];
    let upper = POWER_TO_DELAY_LUT[index + 1];
    lower - (lower - upper) * remainder
}

pub struct PidController {
    kp: f32,
    ki: f32,
    kd: f32,
    setpoint: f32,
    integral: f32,
    prev_error: f32,
    output: f32,
}

impl PidController {
    pub fn new(kp: f32, ki: f32, kd: f32, setpoint: f32) -> Self {
        Self {
            kp,
            ki,
            kd,
            setpoint,
            integral: 0.0,
            prev_error: 0.0,
            output: 0.0,
        }
    }
    pub fn reset(&mut self) {
        self.integral = 0.0;
        self.prev_error = 0.0;
        self.output = 0.0;
    }
    pub fn set_target(&mut self, target: f32) {
        self.setpoint = target;
    }
    pub fn set_coeffs(&mut self, kp: f32, ki: f32, kd: f32) {
        self.kp = kp;
        self.ki = ki;
        self.kd = kd;
    }
    pub fn update(&mut self, measurement: f32) -> f32 {
        let error = self.setpoint - measurement;
        let derivative = error - self.prev_error;
        self.prev_error = error;

        let p_term = self.kp * error;
        let d_term = self.kd * derivative;
        let raw_output = p_term + (self.ki * self.integral) + d_term;

        // Anti-windup: Only accumulate integral if the output isn't saturated,
        // OR if the error is pushing the output away from the saturation boundary.
        if (raw_output > 0.0 && raw_output < 100.0)
            || (raw_output >= 100.0 && error < 0.0)
            || (raw_output <= 0.0 && error > 0.0)
        {
            self.integral += error;
        }

        self.output = (p_term + (self.ki * self.integral) + d_term).clamp(0.0, 100.0);
        self.output
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
    cfg.clock_divider = FixedU32::from_num(125_000_000.0 / 2_000_000.0);
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
    cfg.clock_divider = FixedU32::from_num(125_000_000.0 / 1_000_000.0);
    sm.set_config(&cfg);
    sm.set_pin_dirs(Direction::Out, &[triac_pin]);
    sm.set_enable(true);
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
    let temp_offset = SettingsManager::get().await.temp_offset;

    loop {
        let raw_p = adc.read(&mut ch_p).await.unwrap_or(0) as f32;
        let raw_t = adc.read(&mut ch_t).await.unwrap_or(0) as f32;

        if !initialized {
            p_ema = raw_p;
            t_ema = raw_t;
            initialized = true;
        } else {
            const ALPHA_P: f32 = 0.20;
            const ALPHA_T: f32 = 0.05;
            p_ema = p_ema + ALPHA_P * (raw_p - p_ema);
            t_ema = t_ema + ALPHA_T * (raw_t - t_ema);
        }

        // Convert raw filtered ADC to physical units
        let p_bar = p_ema * (12.0 / 4095.0);
        let t_c = t_ema * (150.0 / 4095.0) + temp_offset;

        // Fetch current state, update it, and broadcast
        let mut state = ADC_WATCH.try_get().unwrap_or(AdcState::default());
        state.pressure_bar = p_bar;
        state.temp_c = t_c;
        ADC_WATCH.sender().send(state);

        ticker.next().await;
    }
}

#[embassy_executor::task]
pub async fn run_unified_hardware_control(
    mut sm_trigger: StateMachine<'static, PIO0, 1>,
    mut sm_triac: StateMachine<'static, PIO0, 2>,
    mut heater: Output<'static>,
) {
    // EMA filter for AC period
    let mut ac_ema = 10_000.0;

    // Load initial settings
    let initial_s = SettingsManager::get().await;
    let mut press_pid = PidController::new(
        initial_s.press_kp,
        initial_s.press_ki,
        initial_s.press_kd,
        0.0,
    );
    let mut temp_pid = PidController::new(
        initial_s.temp_kp,
        initial_s.temp_ki,
        initial_s.temp_kd,
        20.0,
    );

    // Dynamic targets
    let (mut target_p, mut target_t, mut flow_limit) = (0.0, 20.0, 0.0);

    // Heater PWM variables (25-step software PWM)
    let (mut tick, mut duty) = (0u32, 0.0f32);

    loop {
        // --- 1. Zero-Cross Detection & AC Frequency Tracking ---
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

        // --- 2. Sensor Data Retrieval (From ADC Watch) ---
        let state = ADC_WATCH.try_get().unwrap_or(AdcState::default());
        let p_ema = state.pressure_bar;
        let t_ema = state.temp_c;

        let s = SettingsManager::get().await;

        // --- 3. Command & Signal Processing ---
        if let Some(tp) = SIG_TARGET_PRESSURE.try_take() {
            press_pid.set_coeffs(s.press_kp, s.press_ki, s.press_kd);
            press_pid.set_target(tp);
            target_p = tp;
            if target_p == 0.0 {
                press_pid.reset();
            }
        }
        if let Some(fl) = SIG_FLOW_LIMIT.try_take() {
            flow_limit = fl;
        }
        if let Some(tt) = SIG_TARGET_TEMP.try_take() {
            temp_pid.set_coeffs(s.temp_kp, s.temp_ki, s.temp_kd);
            temp_pid.set_target(tt);
            target_t = tt;
        }

        // --- 4. Global Telemetry Update ---
        let mut new_state = state;
        new_state.target_bar = target_p;
        new_state.target_temp = target_t;
        new_state.flow_limit_ml_s = flow_limit;
        new_state.half_wave_us = ac_ema;
        ADC_WATCH.sender().send(new_state);

        // --- 5. Pump Control (Triac Phase Angle) ---
        if target_p > 0.0 {
            let mut p_output = press_pid.update(p_ema);

            // Optional Flow Limiting: reduce pump power if flow exceeds target
            if flow_limit > 0.0 {
                let f = crate::flow_meter::FlowMonitor::new().get_state().await;
                if f.flow_rate_ml_s > flow_limit {
                    p_output = (p_output - ((f.flow_rate_ml_s - flow_limit) * 20.0)).max(0.0);
                }
            }

            // If output is set, push the phase delay to the Triac PIO
            if p_output > 0.0 {
                let delay = get_delay_fraction(p_output) * ac_ema;
                // Subtract small safety margin (250us) to ensure triac turns off before next zero-cross
                sm_triac
                    .tx()
                    .push((delay - 250.0).clamp(10.0, ac_ema - 250.0) as u32);
            }
        }

        // --- 6. Heater Control (Software PWM) ---
        if tick == 0 {
            // PID update for temperature
            let mut t_output = temp_pid.update(t_ema);
            // Feed-forward: if brewing, bump heater power to compensate for cold water inflow
            if target_p > 0.0 {
                t_output = (t_output + 35.0).min(100.0);
            }
            duty = t_output;
        }

        // Execute PWM step
        if (tick as f32) < (duty / 100.0) * 25.0 {
            heater.set_high();
        } else {
            heater.set_low();
        }
        tick = (tick + 1) % 25;
    }
}

pub async fn execute_profile(profile: BrewProfile) {
    defmt::info!("Executing profile: {}", profile.name.as_str());

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
        set_target_pressure(pressure);

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
                    if crate::flow_meter::FlowMonitor::new()
                        .get_state()
                        .await
                        .total_volume_ml
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
    defmt::info!("Profile '{}' completed", profile.name.as_str());
    set_target_pressure(0.0);
}

pub async fn execute_steam() {
    let s = SettingsManager::get().await;
    set_target_temp(s.steam_temp);
    let mut end_timer = pin!(Timer::after(Duration::from_secs(
        s.steam_time_limit_s as u64
    )));
    let monitor = AdcMonitor::new();
    loop {
        let p = monitor.get_state().await.pressure_bar;
        if p < s.steam_pressure {
            set_target_pressure(s.steam_pressure);
        } else {
            set_target_pressure(0.0);
        }

        match select(Timer::after(Duration::from_millis(100)), &mut end_timer).await {
            Either::First(_) => {}
            Either::Second(_) => break,
        }
    }
    set_target_pressure(0.0);
    set_target_temp(s.brew_temp);
}

pub async fn execute_descale() {
    set_target_temp(60.0);
    loop {
        if AdcMonitor::new().get_state().await.temp_c >= 55.0 {
            break;
        }
        Timer::after(Duration::from_millis(500)).await;
    }
    {
        set_target_pressure(2.0);
        loop {
            if crate::flow_meter::FlowMonitor::new()
                .get_state()
                .await
                .total_volume_ml
                >= 500.0
            {
                break;
            }
            Timer::after(Duration::from_millis(100)).await;
        }
        set_target_pressure(0.0);
    }
    Timer::after(Duration::from_secs(10 * 60)).await;
    {
        set_target_pressure(2.0);
        Timer::after(Duration::from_secs(5)).await;
        loop {
            if crate::flow_meter::FlowMonitor::new()
                .get_state()
                .await
                .flow_rate_ml_s
                < 0.5
            {
                break;
            }
            Timer::after(Duration::from_millis(500)).await;
        }
        set_target_pressure(0.0);
    }
    set_target_temp(SettingsManager::get().await.brew_temp);
}
