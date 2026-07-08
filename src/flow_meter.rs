use embassy_futures::select::{select, Either};
use embassy_rp::peripherals::PIO0;
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
    pub total_volume_ml: f32,
    pub shot_volume_ml: f32,
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
    pub fn reset_shot_volume(&self) {
        SIG_RESET_VOLUME.signal(());
    }
}

pub fn setup_flow_sm(
    common: &mut Common<'static, PIO0>,
    sm: &mut StateMachine<'static, PIO0, 0>,
    pio_pin: Pin<'static, PIO0>,
) {
    // High-precision period-measurement with symmetric 2-cycle resolution
    let prg = pio_asm!(
        ".wrap_target",
        // --- 1. MEASURE HIGH STATE ---
        "mov x, !null",
        "high_loop:",
        "jmp x-- next_high", // 1 cycle
        "next_high:",
        "jmp pin high_loop", // 1 cycle
        "mov isr, !x",       // Pin went LOW, invert X
        "push noblock",      // Push HIGH duration
        // --- 2. MEASURE LOW STATE ---
        "mov x, !null",
        "low_loop:",
        "jmp pin low_done", // 1 cycle: breaks out if pin goes HIGH
        "jmp x-- low_loop", // 1 cycle: decrements and loops if X != 0
        "jmp low_loop",     // catch the fall-through and loop back
        "low_done:",
        "mov isr, !x",
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
pub async fn run_flow_task(mut sm: StateMachine<'static, PIO0, 0>) {
    let mut total_edges = 0u32;
    let mut edges_at_start = 0u32;

    let s = crate::settings::Settings::get().await;
    let edges_per_liter = if s.hardware.flow_edges_per_liter > 0.0 {
        s.hardware.flow_edges_per_liter
    } else {
        5200.0 // Fallback to avoid division by zero
    };
    let ml_per_edge: f32 = 1000.0 / edges_per_liter;
    let flow_numerator: f32 = (CLOCK_FREQ_HZ / CYCLES_PER_LOOP) * ml_per_edge;

    loop {
        match with_timeout(
            Duration::from_millis(200),
            select(sm.rx().wait_pull(), SIG_RESET_VOLUME.wait()),
        )
        .await
        {
            Ok(Either::First(val)) => {
                let mut current_ticks = val;
                total_edges += 1;

                // Drain the FIFO (cap at 4 to prevent infinite loop on noise storm)
                for _ in 0..4 {
                    if let Some(val) = sm.rx().try_pull() {
                        total_edges += 1;
                        current_ticks = val;
                        defmt::warn!("PIO FIFO had multiple entries!");
                    } else {
                        break;
                    }
                }

                if current_ticks > 0 {
                    let raw_flow_ml_s = flow_numerator / (current_ticks as f32);
                    let vol_ml = (total_edges as f32) * ml_per_edge;
                    let shot_ml = (total_edges.wrapping_sub(edges_at_start) as f32) * ml_per_edge;

                    let mut state = FLOW_WATCH.try_get().unwrap_or_default();

                    const ALPHA: f32 = 0.3; // EMA filter coefficient
                    if state.flow_rate_ml_s == 0.0 {
                        state.flow_rate_ml_s = raw_flow_ml_s;
                    } else {
                        state.flow_rate_ml_s =
                            state.flow_rate_ml_s + ALPHA * (raw_flow_ml_s - state.flow_rate_ml_s);
                    }

                    state.total_volume_ml = vol_ml;
                    state.shot_volume_ml = shot_ml;
                    FLOW_WATCH.sender().send(state);
                } else {
                    defmt::warn!("PIO return 0 for flow");
                }
            }
            Ok(Either::Second(_)) => {
                edges_at_start = total_edges;
                let mut state = FLOW_WATCH.try_get().unwrap_or_default();
                state.shot_volume_ml = 0.0;
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
