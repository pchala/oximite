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

pub const CYCLES_PER_LOOP: f32 = 2.0;

/// Counts to add to each PIO half-period measurement to account for the cycles
/// the state machine spends *outside* its counting loops.
///
/// Per full pulse that is 8 cycles: the entry dispatch (1), `mov x, !null` ×2,
/// `mov isr, !x` ×2, `push noblock` ×2, and one extra `jmp pin` in the LOW loop,
/// which checks before decrementing and so runs its test `n+1` times against
/// the HIGH loop's `n`. At 2 cycles per count that is 4 counts, split evenly
/// across the two half-period samples, giving `2` each and making
/// `(n_high + 2) + (n_low + 2)` equal exactly half the true pulse period.
///
/// The correction is exact but small — under 0.001 % at espresso flow rates,
/// far below the sensor's own ±2 % spec — so it matters for the model being
/// right rather than for the reading being usefully different.
const PIO_TICK_OFFSET: u32 = 2;

#[derive(Clone, Copy, Default)]
pub struct FlowState {
    pub flow_rate_ml_s: f32,
    pub volume_ml: f32,
}

pub static FLOW_WATCH: Watch<CriticalSectionRawMutex, FlowState, 4> = Watch::new();

/// The latest flow reading, or zeroes if the flow task hasn't published yet.
pub fn get_flow() -> FlowState {
    FLOW_WATCH.try_get().unwrap_or_default()
}

/// Zeroes the accumulated volume. Applied by `run_flow_task` rather than here,
/// so the reset lands between two pulse counts instead of racing one.
pub fn reset_volume() {
    SIG_RESET_VOLUME.signal(());
}

const RATE_WINDOW_LEN: usize = 32;
const RATE_WINDOW_MASK: usize = RATE_WINDOW_LEN - 1;

/// One mains period expressed in flow-PIO ticks. Each tick is
/// `CYCLES_PER_LOOP` cycles at `SYS_CLK_HZ`, so 20 ms = 0.02 * 75e6.
/// Sized for 50 Hz; on 60 Hz mains this spans 1.2 pump strokes instead of
/// exactly 1, which weakens ripple rejection but nothing else.
const RATE_WINDOW_TICKS: u64 = 1_500_000;

/// Sliding window of recent half-period measurements, bounded by elapsed time
/// rather than by sample count.
///
/// Bounding by time is the point: a fixed-length average has a window whose
/// *duration* scales with 1/flow, so its lag and its ripple rejection both
/// swing with flow and the control loop's phase margin moves under it. Holding
/// the duration at one mains period keeps the lag constant and lines the window
/// up with the pump's stroke, so the 50 Hz delivery ripple averages out.
///
/// It sums *ticks*, not per-pulse rates. Each edge marks an equal volume, not
/// an equal interval, so a mean of per-pulse rates is volume-weighted and
/// over-reads whenever delivery is pulsatile (+10 % at ±30 % period ripple).
/// `n * ml_per_edge / sum_ticks` is volume over elapsed time — the same
/// quantity the volume counter integrates, so rate and volume cannot disagree.
struct RateWindow {
    ticks: [u32; RATE_WINDOW_LEN],
    head: usize,
    len: usize,
    sum: u64,
}

impl RateWindow {
    const fn new() -> Self {
        Self {
            ticks: [0; RATE_WINDOW_LEN],
            head: 0,
            len: 0,
            sum: 0,
        }
    }

    /// Drops the whole window. Used whenever a gap makes the buffered periods
    /// stale — they'd otherwise be averaged against samples from another shot.
    fn clear(&mut self) {
        self.head = 0;
        self.len = 0;
        self.sum = 0;
    }

    fn tail(&self) -> usize {
        (self.head + RATE_WINDOW_LEN - self.len) & RATE_WINDOW_MASK
    }

    fn push(&mut self, ticks: u32) {
        // Full before the window filled (>16 ml/s): drop the oldest regardless,
        // shortening the window rather than refusing the sample.
        if self.len == RATE_WINDOW_LEN {
            self.sum -= self.ticks[self.tail()] as u64;
            self.len -= 1;
        }
        self.ticks[self.head] = ticks;
        self.head = (self.head + 1) & RATE_WINDOW_MASK;
        self.len += 1;
        self.sum += ticks as u64;

        // Shrink from the tail while the window still spans a full mains period
        // without its oldest sample. Keeps at least one, so flows too slow to
        // fill the window degrade to plain reciprocal timing instead of failing.
        while self.len > 1 {
            let oldest = self.ticks[self.tail()] as u64;
            if self.sum - oldest < RATE_WINDOW_TICKS {
                break;
            }
            self.sum -= oldest;
            self.len -= 1;
        }
    }

    fn rate(&self, flow_numerator: f32) -> Option<f32> {
        if self.len == 0 || self.sum == 0 {
            return None;
        }
        Some(flow_numerator * self.len as f32 / self.sum as f32)
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
        "jmp pin high_phase", // pin HIGH — time the HIGH phase
        "jmp low_phase",      // pin LOW  — time the LOW phase
        // --- 1. MEASURE HIGH PHASE ---
        "high_phase:",
        "mov x, !null",
        "high_loop:",
        "jmp x-- next_high", // 1 cycle
        "next_high:",
        "jmp pin high_loop", // 1 cycle: loop while HIGH
        "mov isr, !x",       // pin went LOW — push HIGH duration
        "push noblock",
        // --- 2. MEASURE LOW PHASE ---
        "low_phase:",
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
    let mut window = RateWindow::new();
    // A half-period that spans an idle gap (start of a shot, or any pause
    // longer than the 200 ms timeout) carries a stale tick count. Drop its
    // *rate*; the edge itself is a real pulse and still counts towards volume.
    let mut skip_stale_rate = false;

    let s = crate::settings::Settings::get().await;
    let pulses_per_liter = if s.machine.flow_pulses_per_liter > 0.0 {
        s.machine.flow_pulses_per_liter
    } else {
        98324.0 // 49162 physical pulses/L × 2 edges; fallback if flash value is 0
    };
    let ml_per_pulse: f32 = 1000.0 / pulses_per_liter;
    let flow_numerator: f32 = (crate::board::SYS_CLK_HZ / CYCLES_PER_LOOP) * ml_per_pulse;

    loop {
        match with_timeout(
            Duration::from_millis(200),
            select(sm.rx().wait_pull(), SIG_RESET_VOLUME.wait()),
        )
        .await
        {
            Ok(Either::First(first_val)) => {
                let mut valid_pulses: u32 = 0;
                let mut total_pulses = 1;

                // Scoped so the closure's mutable borrow of `window` ends
                // before the read below.
                {
                    let mut process_pulse = |val: u32, use_for_rate: bool| {
                        if val == 0 {
                            defmt::warn!("PIO return 0 for flow");
                            return;
                        }
                        if !use_for_rate {
                            defmt::info!("Skipped stale flow rate sample from idle state");
                            valid_pulses += 1;
                            return;
                        }
                        let ticks = val + PIO_TICK_OFFSET;
                        let raw_flow = flow_numerator / (ticks as f32);
                        if raw_flow <= 50.0 {
                            valid_pulses += 1;
                            window.push(ticks);
                        } else {
                            defmt::warn!("Ignored noise pulse ({} ml/s)", raw_flow);
                        }
                    };

                    process_pulse(first_val, !skip_stale_rate);
                    skip_stale_rate = false;

                    // Drain any extra entries that piled up in the FIFO.
                    while let Some(val) = sm.rx().try_pull() {
                        total_pulses += 1;
                        process_pulse(val, true);
                    }
                }

                if valid_pulses > 0 {
                    if total_pulses > 1 {
                        defmt::warn!("PIO FIFO had {} entries!", total_pulses);
                    }

                    volume_ml += ml_per_pulse * valid_pulses as f32;

                    let mut state = FLOW_WATCH.try_get().unwrap_or_default();

                    if let Some(r) = window.rate(flow_numerator) {
                        state.flow_rate_ml_s = r;
                    }

                    state.volume_ml = volume_ml;
                    FLOW_WATCH.sender().send(state);
                }
            }
            Ok(Either::Second(_)) => {
                volume_ml = 0.0;
                skip_stale_rate = true;
                window.clear();
                let mut state = FLOW_WATCH.try_get().unwrap_or_default();
                state.volume_ml = 0.0;
                state.flow_rate_ml_s = 0.0;
                FLOW_WATCH.sender().send(state);
            }
            Err(_) => {
                skip_stale_rate = true;
                window.clear();
                let mut state = FLOW_WATCH.try_get().unwrap_or_default();
                state.flow_rate_ml_s = 0.0;
                FLOW_WATCH.sender().send(state);
            }
        }
    }
}
