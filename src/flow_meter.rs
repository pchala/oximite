use embassy_futures::select::{select, Either};
use embassy_rp::peripherals::PIO2;
use embassy_rp::pio::{Common, Config, FifoJoin, Pin, StateMachine};
use embassy_sync::watch::Watch;
use embassy_time::{with_timeout, Duration};
use fixed::FixedU32;
use pio::pio_asm;

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;

pub static SIG_RESET_VOLUME: Signal<CriticalSectionRawMutex, ()> = Signal::new();

pub const CLOCK_FREQ_HZ: f32 = 150_000_000.0;
pub const CYCLES_PER_LOOP: f32 = 2.0;

#[derive(Clone, Copy, Default)]
pub struct FlowState {
    pub flow_rate_ml_s: f32,
    pub volume_ml: f32,
}

pub static FLOW_WATCH: Watch<CriticalSectionRawMutex, FlowState, 4> = Watch::new();

pub struct FlowMonitor;
impl FlowMonitor {
    pub fn new() -> Self {
        Self
    }
    pub async fn get_state(&self) -> FlowState {
        FLOW_WATCH.try_get().unwrap_or_default()
    }
    pub fn reset_volume(&self) {
        SIG_RESET_VOLUME.signal(());
    }
}

pub fn setup_flow_sm(
    common: &mut Common<'static, PIO2>,
    sm: &mut StateMachine<'static, PIO2, 0>,
    pio_pin: Pin<'static, PIO2>,
) {
    // Measures each half-period separately and pushes twice per physical
    // pulse (once for HIGH, once for LOW). With a 50% duty-cycle sensor
    // this doubles the effective sampling rate vs. measuring full periods.
    // CYCLES_PER_LOOP = 2 (jmp x-- + jmp pin, one 2-cycle iteration each).
    let prg = pio_asm!(
        ".wrap_target",
        // --- 1. MEASURE HIGH PHASE ---
        "mov x, !null",
        "high_loop:",
        "jmp x-- next_high", // 1 cycle
        "next_high:",
        "jmp pin high_loop", // 1 cycle: loop while HIGH
        "mov isr, !x",       // pin went LOW — push HIGH duration
        "push noblock",
        // --- 2. MEASURE LOW PHASE ---
        "mov x, !null",
        "low_loop:",
        "jmp pin low_done", // 1 cycle: exit when HIGH
        "jmp x-- low_loop", // 1 cycle
        "jmp low_loop",     // catch fall-through (X wrapped)
        "low_done:",
        "mov isr, !x", // pin went HIGH — push LOW duration
        "push noblock",
        ".wrap",
    );
    let loaded = common.load_program(&prg.program);
    let mut cfg = Config::default();
    cfg.use_program(&loaded, &[]);
    cfg.set_in_pins(&[&pio_pin]);
    cfg.set_jmp_pin(&pio_pin);
    cfg.fifo_join = FifoJoin::RxOnly;
    cfg.clock_divider = FixedU32::from_num(1.0);
    sm.set_config(&cfg);
    sm.set_enable(true);
}

#[embassy_executor::task]
pub async fn run_flow_task(mut sm: StateMachine<'static, PIO2, 0>) {
    let mut volume_ml: f32 = 0.0;

    let s = crate::settings::Settings::get().await;
    let pulses_per_liter = if s.machine.flow_pulses_per_liter > 0.0 {
        s.machine.flow_pulses_per_liter
    } else {
        98324.0 // 49162 physical pulses/L × 2 edges; fallback if flash value is 0
    };
    let ml_per_pulse: f32 = 1000.0 / pulses_per_liter;
    let flow_numerator: f32 = (CLOCK_FREQ_HZ / CYCLES_PER_LOOP) * ml_per_pulse;

    loop {
        match with_timeout(
            Duration::from_millis(200),
            select(sm.rx().wait_pull(), SIG_RESET_VOLUME.wait()),
        )
        .await
        {
            Ok(Either::First(first_val)) => {
                let mut valid_pulses: u32 = 0;
                let mut last_valid_ticks: u32 = 0;
                let mut total_pulses = 1;

                let mut process_pulse = |val: u32| {
                    if val > 0 {
                        let raw_flow = flow_numerator / (val as f32);
                        if raw_flow <= 50.0 {
                            valid_pulses += 1;
                            last_valid_ticks = val;
                        } else {
                            defmt::warn!("Ignored noise pulse ({} ml/s)", raw_flow);
                        }
                    } else {
                        defmt::warn!("PIO return 0 for flow");
                    }
                };

                process_pulse(first_val);

                // Drain any extra entries that piled up in the FIFO.
                while let Some(val) = sm.rx().try_pull() {
                    total_pulses += 1;
                    process_pulse(val);
                }

                if valid_pulses > 0 {
                    let raw_flow_ml_s = flow_numerator / (last_valid_ticks as f32);
                    if total_pulses > 1 {
                        defmt::warn!("PIO FIFO had {} entries!", total_pulses);
                    }

                    volume_ml += ml_per_pulse * valid_pulses as f32;

                    let mut state = FLOW_WATCH.try_get().unwrap_or_default();

                    const ALPHA: f32 = 0.3; // EMA filter coefficient
                    if state.flow_rate_ml_s == 0.0 {
                        state.flow_rate_ml_s = raw_flow_ml_s;
                    } else {
                        state.flow_rate_ml_s =
                            state.flow_rate_ml_s + ALPHA * (raw_flow_ml_s - state.flow_rate_ml_s);
                    }

                    state.volume_ml = volume_ml;
                    FLOW_WATCH.sender().send(state);
                }
            }
            Ok(Either::Second(_)) => {
                volume_ml = 0.0;
                let mut state = FLOW_WATCH.try_get().unwrap_or_default();
                state.volume_ml = 0.0;
                state.flow_rate_ml_s = 0.0;
                FLOW_WATCH.sender().send(state);
            }
            Err(_) => {
                let mut state = FLOW_WATCH.try_get().unwrap_or_default();
                state.flow_rate_ml_s = 0.0;
                FLOW_WATCH.sender().send(state);
            }
        }
    }
}
