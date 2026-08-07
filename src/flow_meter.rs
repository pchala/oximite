use embassy_futures::select::{select, Either};
use embassy_rp::peripherals::PIO2;
use embassy_rp::pio::{Common, Config, FifoJoin, Pin, StateMachine};
use embassy_time::{with_timeout, Duration};
use fixed::FixedU32;
use pio::pio_asm;

use core::cell::{Cell, RefCell};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::blocking_mutex::Mutex as BlockingMutex;
use embassy_sync::signal::Signal;

pub static SIG_RESET_VOLUME: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// Accumulated shot volume, in ml.
///
/// Shared, not a task-local: `reset_volume` must take effect before its
/// caller's next read, and `coordinator::start` resets immediately before the
/// profile's first step compares volume against its target.
static VOLUME_ML: BlockingMutex<CriticalSectionRawMutex, Cell<f32>> =
    BlockingMutex::new(Cell::new(0.0));

/// Volume accumulated since the last [`reset_volume`], in ml.
pub fn shot_volume_ml() -> f32 {
    VOLUME_ML.lock(|v| v.get())
}

pub const CYCLES_PER_LOOP: f32 = 2.0;

/// Counts to add to each PIO half-period to account for the cycles the state
/// machine spends outside its counting loops: 4 counts per pulse, split evenly
/// so `(n_high + 2) + (n_low + 2)` is exactly the true pulse period.
const PIO_TICK_OFFSET: u32 = 2;

/// Zeroes the accumulated volume, synchronously. The signal additionally tells
/// the flow task to drop the half-period spanning the idle gap.
pub fn reset_volume() {
    VOLUME_ML.lock(|v| v.set(0.0));
    SIG_RESET_VOLUME.signal(());
}

/// Number of edges averaged for the rate. Must be even: a HIGH plus a LOW span
/// exactly one magnet pass, so an even count cancels the sensor's duty
/// asymmetry instead of alternating a bias into the reading.
const RATE_EDGES: usize = 4;
const RATE_EDGES_MASK: usize = RATE_EDGES - 1;

/// The last few half-period measurements, averaged to give the flow rate.
struct RateWindow {
    ticks: [u32; RATE_EDGES],
    head: usize,
    len: usize,
}

impl RateWindow {
    const fn new() -> Self {
        Self {
            ticks: [0; RATE_EDGES],
            head: 0,
            len: 0,
        }
    }

    /// Drops the window when a gap makes the buffered periods stale.
    fn clear(&mut self) {
        self.head = 0;
        self.len = 0;
    }

    fn push(&mut self, ticks: u32) {
        self.ticks[self.head] = ticks;
        self.head = (self.head + 1) & RATE_EDGES_MASK;
        if self.len < RATE_EDGES {
            self.len += 1;
        }
    }

    fn rate(&self, flow_numerator: f32) -> Option<f32> {
        // Round down to an even count so a part-filled buffer stays unbiased.
        let n = self.len & !1;
        if n == 0 {
            return None;
        }
        let mut sum: u64 = 0;
        for k in 0..n {
            sum += self.ticks[(self.head + RATE_EDGES - 1 - k) & RATE_EDGES_MASK] as u64;
        }
        if sum == 0 {
            return None;
        }
        Some(flow_numerator * n as f32 / sum as f32)
    }
}

struct RateState {
    window: RateWindow,
    /// `tick_rate_hz * ml_per_edge`; set once the flow task has read settings.
    numerator: f32,
    /// Last computed rate, held while the buffer is refilling after a `clear`.
    /// A cleared window must not read as zero flow and step the PID; only the
    /// timeout and a reset mean "not flowing".
    last: f32,
}

static RATE: BlockingMutex<CriticalSectionRawMutex, RefCell<RateState>> =
    BlockingMutex::new(RefCell::new(RateState {
        window: RateWindow::new(),
        numerator: 0.0,
        last: 0.0,
    }));

/// The current flow rate in ml/s, evaluated at the moment of the call.
///
/// Pulled once per control tick rather than pushed on every edge, so the value
/// the controller acts on is the one telemetry logs.
pub fn flow_rate_ml_s() -> f32 {
    RATE.lock(|r| {
        let r = &mut *r.borrow_mut();
        match r.window.rate(r.numerator) {
            Some(v) => {
                r.last = v;
                v
            }
            None => r.last,
        }
    })
}

fn window_push(ticks: u32) {
    RATE.lock(|r| r.borrow_mut().window.push(ticks));
}

/// Drops the buffered periods but keeps the last reported rate.
fn window_clear() {
    RATE.lock(|r| r.borrow_mut().window.clear());
}

/// Clears the window and reports zero, for cases where the pump really stopped.
fn window_reset() {
    RATE.lock(|r| {
        let r = &mut *r.borrow_mut();
        r.window.clear();
        r.last = 0.0;
    });
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
    RATE.lock(|r| r.borrow_mut().numerator = flow_numerator);

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

                let mut process_pulse = |val: u32, use_for_rate: bool| {
                    // A runt or an implausibly short interval means a spurious
                    // edge split a real pulse, so the samples either side of it
                    // are corrupt too — drop the whole window rather than
                    // average a poisoned neighbourhood.
                    if val == 0 {
                        defmt::warn!("PIO return 0 for flow");
                        window_clear();
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
                        window_push(ticks);
                    } else {
                        defmt::warn!("Ignored noise pulse ({} ml/s)", raw_flow);
                        window_clear();
                    }
                };

                process_pulse(first_val, !skip_stale_rate);
                skip_stale_rate = false;

                // Drain any extra entries that piled up in the FIFO.
                while let Some(val) = sm.rx().try_pull() {
                    total_pulses += 1;
                    process_pulse(val, true);
                }

                if valid_pulses > 0 {
                    if total_pulses > 1 {
                        defmt::warn!("PIO FIFO had {} entries!", total_pulses);
                    }

                    let added = ml_per_pulse * valid_pulses as f32;
                    VOLUME_ML.lock(|v| v.set(v.get() + added));
                }
            }
            Ok(Either::Second(_)) => {
                // `reset_volume` already zeroed the counter. What is left is
                // the state only this task owns: dropping the half-period that
                // spans the gap.
                skip_stale_rate = true;
                window_reset();
            }
            Err(_) => {
                skip_stale_rate = true;
                window_reset();
            }
        }
    }
}
