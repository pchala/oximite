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
pub static SIG_DIRECT_PUMP: Signal<CriticalSectionRawMutex, Option<f32>> = Signal::new();

#[derive(Clone)]
pub enum HardwareCommand {
    RunProfile(BrewProfile),
    Steam,
    Descale,
    DirectPump(f32),
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
pub fn set_direct_pump(power: Option<f32>) {
    SIG_DIRECT_PUMP.signal(power);
}

#[derive(Clone, Copy, Default)]
pub struct AdcState {
    pub pressure_bar: f32,
    pub temp_c: f32,
    pub target_bar: f32,
    pub target_temp: f32,
    pub flow_limit_ml_s: f32,
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
        })
    }
}

const NTC_LUT: [f32; 129] = [
    150.00, 149.62, 148.18, 146.78, 145.43, 144.11, 142.84, 141.60, 140.39, 139.21, 138.07, 136.95,
    135.86, 134.79, 133.75, 132.74, 131.74, 130.77, 129.82, 128.88, 127.97, 127.07, 126.19, 125.33,
    124.48, 123.65, 122.83, 122.03, 121.24, 120.47, 119.70, 118.95, 118.21, 117.48, 116.77, 116.06,
    115.36, 114.68, 114.00, 113.33, 112.67, 112.02, 111.38, 110.75, 110.12, 109.50, 108.89, 108.29,
    107.69, 107.10, 106.52, 105.94, 105.37, 104.81, 104.25, 103.70, 103.15, 102.61, 102.08, 101.55,
    101.02, 100.50, 99.99, 99.48, 98.97, 98.47, 97.97, 97.48, 96.99, 96.50, 96.02, 95.55, 95.07,
    94.60, 94.14, 93.67, 93.22, 92.76, 92.31, 91.86, 91.41, 90.97, 90.53, 90.09, 89.66, 89.23,
    88.80, 88.37, 87.95, 87.53, 87.11, 86.69, 86.28, 85.87, 85.46, 85.05, 84.65, 84.24, 83.84,
    83.45, 83.05, 82.66, 82.26, 81.87, 81.48, 81.10, 80.71, 80.33, 79.95, 79.56, 79.19, 78.81,
    78.43, 78.06, 77.69, 77.31, 76.94, 76.58, 76.21, 75.84, 75.48, 75.11, 74.75, 74.39, 74.03,
    73.67, 73.31, 72.95, 72.60,
];

fn get_temp_from_adc(raw_adc: f32) -> f32 {
    if raw_adc < 400.0 {
        return NTC_LUT[0];
    }
    let index_f = (raw_adc - 400.0) / 12.0;
    let index = index_f as usize;
    if index >= 128 {
        return NTC_LUT[128];
    }
    let remainder = index_f - (index as f32);
    let lower = NTC_LUT[index];
    let upper = NTC_LUT[index + 1];
    lower + (upper - lower) * remainder
}

const POWER_TO_DELAY_LUT: [f32; 101] = [
    0.6000, 0.5964, 0.5929, 0.5894, 0.5859, 0.5825, 0.5790, 0.5756, 0.5722, 0.5688,
    0.5654, 0.5621, 0.5587, 0.5554, 0.5521, 0.5488, 0.5455, 0.5422, 0.5389, 0.5357,
    0.5324, 0.5291, 0.5259, 0.5226, 0.5194, 0.5162, 0.5129, 0.5097, 0.5065, 0.5033,
    0.5000, 0.4968, 0.4936, 0.4904, 0.4871, 0.4839, 0.4807, 0.4774, 0.4742, 0.4709,
    0.4677, 0.4644, 0.4612, 0.4579, 0.4546, 0.4513, 0.4480, 0.4447, 0.4413, 0.4380,
    0.4346, 0.4313, 0.4279, 0.4245, 0.4210, 0.4176, 0.4141, 0.4107, 0.4072, 0.4036,
    0.4001, 0.3965, 0.3929, 0.3893, 0.3856, 0.3819, 0.3782, 0.3744, 0.3706, 0.3668,
    0.3629, 0.3590, 0.3550, 0.3510, 0.3469, 0.3427, 0.3386, 0.3343, 0.3300, 0.3256,
    0.3211, 0.3166, 0.3120, 0.3072, 0.3024, 0.2975, 0.2924, 0.2873, 0.2820, 0.2765,
    0.2709, 0.2651, 0.2591, 0.2529, 0.2464, 0.2397, 0.2326, 0.2252, 0.2174, 0.2090,
    0.2000,
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
    pub setpoint: f32,
    integral: f32,
    prev_error: f32,
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
        }
    }
    pub fn reset(&mut self) {
        self.integral = 0.0;
        self.prev_error = 0.0;
    }
    pub fn set_target(&mut self, target: f32) {
        self.setpoint = target;
    }
    pub fn set_coeffs(&mut self, kp: f32, ki: f32, kd: f32) {
        self.kp = kp;
        self.ki = ki;
        self.kd = kd;
    }
    pub fn update(&mut self, measurement: f32, accumulate: bool) -> f32 {
        const DT: f32 = 0.02; // 50 Hz loop rate (1 / 50)

        let error = self.setpoint - measurement;
        let derivative = (error - self.prev_error) / DT;
        self.prev_error = error;

        let p_term = self.kp * error;
        let d_term = self.kd * derivative;
        let mut output = p_term + (self.ki * self.integral) + d_term;

        if accumulate {
            // Internal anti-windup: only integrate if not saturated at typical 0-100 limits
            // or if we are moving back towards the linear range.
            if (output > 0.0 && output < 100.0)
                || (output >= 100.0 && error < 0.0)
                || (output <= 0.0 && error > 0.0)
            {
                self.integral += error * DT;
                output += self.ki * error * DT;
            }
        }

        output
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
    let temp_offset = SettingsManager::get().await.hardware.temp_offset;

    loop {
        let raw_p = adc.read(&mut ch_p).await.unwrap_or(0) as f32;
        let raw_t = adc.read(&mut ch_t).await.unwrap_or(0) as f32;

        if !initialized {
            p_ema = raw_p;
            t_ema = raw_t;
            initialized = true;
        } else {
            const ALPHA_P: f32 = 0.05; // 4.0 Hz Cutoff (Attenuates 50Hz pump ripple)
            const ALPHA_T: f32 = 0.01; // 0.8 Hz Cutoff (Rock solid thermal readings)
            p_ema = p_ema + ALPHA_P * (raw_p - p_ema);
            t_ema = t_ema + ALPHA_T * (raw_t - t_ema);
        }

        // Convert raw filtered ADC to physical units
        let p_bar = p_ema * (12.0 / 4095.0);
        let t_c = get_temp_from_adc(t_ema) + temp_offset;

        // Fetch current state, update it, and broadcast
        let mut state = ADC_WATCH.try_get().unwrap_or(AdcState::default());
        state.pressure_bar = p_bar;
        state.temp_c = t_c;
        ADC_WATCH.sender().send(state);

        ticker.next().await;
    }
}

#[embassy_executor::task]
pub async fn pump_control_task(
    mut sm_trigger: StateMachine<'static, PIO0, 1>,
    mut sm_triac: StateMachine<'static, PIO0, 2>,
) {
    // EMA filter for AC period
    let mut ac_ema = 10_000.0;

    // Load initial settings
    let initial_s = SettingsManager::get().await;
    let mut press_pid = PidController::new(
        initial_s.press_pid.kp,
        initial_s.press_pid.ki,
        initial_s.press_pid.kd,
        0.0,
    );

    // Dynamic targets
    let (mut target_p, mut flow_limit) = (0.0, 0.0);
    let mut direct_pump: Option<f32> = None;

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

        let s = SettingsManager::get().await;

        // --- 3. Command & Signal Processing ---
        if let Some(tp) = SIG_TARGET_PRESSURE.try_take() {
            press_pid.set_coeffs(s.press_pid.kp, s.press_pid.ki, s.press_pid.kd);
            press_pid.set_target(tp);
            target_p = tp;
            if target_p == 0.0 {
                press_pid.reset();
            }
        }
        if let Some(fl) = SIG_FLOW_LIMIT.try_take() {
            flow_limit = fl;
        }
        if let Some(dp) = SIG_DIRECT_PUMP.try_take() {
            direct_pump = dp;
        }

        // --- 4. Global Telemetry Update ---
        let mut new_state = state;
        new_state.target_bar = target_p;
        new_state.flow_limit_ml_s = flow_limit;
        ADC_WATCH.sender().send(new_state);

        let mut p_output: f32 = 0.0;

        // --- 5. Pump Control (Triac Phase Angle) ---
        if let Some(dp) = direct_pump {
            p_output = dp.clamp(0.0, 100.0);
        } else if target_p > 0.0 {
            let mut flow_over = 0.0;

            // reduce pump power if flow exceeds target
            if flow_limit > 0.0 {
                let f = crate::flow_meter::FlowMonitor::new().get_state().await;
                if f.flow_rate_ml_s > flow_limit {
                    flow_over = (f.flow_rate_ml_s - flow_limit) * s.hardware.flow_multiplier;
                }
            }

            let p_output_raw = press_pid.update(p_ema, flow_over == 0.0);
            p_output = (p_output_raw - flow_over).clamp(0.0, 100.0);
        }

        // If output is set, push the phase delay to the Triac PIO
        if p_output > 0.0 {
            let delay = get_delay_fraction(p_output) * ac_ema;
            sm_triac.tx().push(delay as u32);
        }
    }
}

#[embassy_executor::task]
pub async fn heater_control_task(mut heater: Output<'static>) {
    // Load initial settings
    let initial_s = SettingsManager::get().await;
    let mut temp_pid = PidController::new(
        initial_s.temp_pid.kp,
        initial_s.temp_pid.ki,
        initial_s.temp_pid.kd,
        0.0,
    );

    // Dynamic targets
    let mut target_t = 20.0;

    // Heater PWM variables (25-step software PWM)
    let (mut tick, mut duty) = (0u32, 0.0f32);

    // Ticker for 25Hz execution (since PWM relies on 25 steps per cycle)
    let mut ticker = embassy_time::Ticker::every(Duration::from_hz(25));

    loop {
        // --- 1. Sensor Data Retrieval (From ADC Watch) ---
        let state = ADC_WATCH.try_get().unwrap_or(AdcState::default());
        let t_ema = state.temp_c;

        let s = SettingsManager::get().await;

        // --- 2. Command & Signal Processing ---
        if let Some(tt) = SIG_TARGET_TEMP.try_take() {
            temp_pid.set_coeffs(s.temp_pid.kp, s.temp_pid.ki, s.temp_pid.kd);
            temp_pid.set_target(tt);
            target_t = tt;
        }

        // --- 3. Global Telemetry Update ---
        let mut new_state = state;
        new_state.target_temp = target_t;
        ADC_WATCH.sender().send(new_state);

        // --- 4. Heater Control (Software PWM) ---
        if tick == 0 {
            let target_p = state.target_bar; // Retrieve pressure target for feed-forward logic

            // PID update for temperature
            let feed_forward = if target_p > 0.0 {
                s.hardware.temp_feed_forward
            } else {
                0.0
            };

            let t_output_raw = temp_pid.update(t_ema, feed_forward == 0.0);
            duty = (t_output_raw + feed_forward).clamp(0.0, 100.0);
        }

        // Execute PWM step
        if (tick as f32) < (duty / 100.0) * 25.0 {
            heater.set_high();
        } else {
            heater.set_low();
        }
        tick = (tick + 1) % 25;

        ticker.next().await;
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
    set_target_temp(s.machine.steam_temp);
    let mut end_timer = pin!(Timer::after(Duration::from_secs(
        s.machine.steam_time_limit_s as u64
    )));
    let monitor = AdcMonitor::new();
    loop {
        let p = monitor.get_state().await.pressure_bar;
        if p < s.machine.steam_pressure {
            set_target_pressure(s.machine.steam_pressure);
        } else {
            set_target_pressure(0.0);
        }

        match select(Timer::after(Duration::from_millis(100)), &mut end_timer).await {
            Either::First(_) => {}
            Either::Second(_) => break,
        }
    }
    set_target_pressure(0.0);
    set_target_temp(s.machine.brew_temp);
}

pub async fn execute_descale() {
    set_target_temp(60.0);

    loop {
        set_direct_pump(Some(30.0f32));
        Timer::after(Duration::from_millis(500)).await;
        set_direct_pump(Some(0.0f32));
        // until flow stops
        if crate::flow_meter::FlowMonitor::new()
            .get_state()
            .await
            .flow_rate_ml_s
            < 0.5
        {
            break;
        }
        Timer::after(Duration::from_secs(1)).await;
    }

    set_direct_pump(None);
    set_target_temp(SettingsManager::get().await.machine.brew_temp);
}

pub async fn execute_direct_pump(power: f32) {
    set_direct_pump(Some(power));
    core::future::pending::<()>().await;
}
