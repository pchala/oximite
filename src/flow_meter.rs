use embassy_futures::select::{select, Either};
use embassy_rp::peripherals::PIO2;
use embassy_rp::pio::{Common, Config, FifoJoin, Pin, StateMachine};
use embassy_time::{with_timeout, Duration};
use fixed::FixedU32;
use pio::pio_asm;

use core::cell::Cell;
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

/// Number of half-period samples averaged for the rate.
///
/// A single half-period is already a usable reading, so this is purely a
/// noise/lag trade: more samples smooth the sensor's quantisation and, once
/// the window spans whole pulses (a HIGH plus a LOW is one magnet pass),
/// cancel its duty asymmetry. Four is the sweet spot. Free to tune — neither
/// an even count nor a power of two is required.
const RATE_EDGES: usize = 4;

/// The last few half-period measurements, averaged to give the flow rate.
///
/// Task-local to `run_flow_task`, which is its only writer; only the resulting
/// rate is shared, via [`RATE_ML_S`].
struct RateWindow {
    ticks: [u32; RATE_EDGES],
    head: usize,
    len: usize,
}

impl RateWindow {
    fn new() -> Self {
        Self {
            ticks: [0; RATE_EDGES],
            head: 0,
            len: 0,
        }
    }

    /// Drops the window when a gap makes the buffered periods stale. The last
    /// published rate stands until the next sample refills it: a cleared
    /// window must not read as zero flow and step the PID.
    fn clear(&mut self) {
        self.head = 0;
        self.len = 0;
    }

    /// The pump really stopped — drop the window *and* report zero.
    fn stop(&mut self) {
        self.clear();
        publish_rate(0.0);
    }

    fn push(&mut self, ticks: u32) {
        self.ticks[self.head] = ticks;
        self.head = (self.head + 1) % RATE_EDGES;
        self.len = (self.len + 1).min(RATE_EDGES);
    }

    /// `clear` resets `head` to 0, so a partial window always occupies
    /// `ticks[..len]` and a full one the whole array
    /// `None` only when the window is empty: a pushed sample is never 0.
    fn rate(&self, flow_numerator: f32) -> Option<f32> {
        let s = &self.ticks[..self.len];
        let sum: u64 = s.iter().map(|&t| t as u64).sum();
        (sum > 0).then(|| flow_numerator * s.len() as f32 / sum as f32)
    }

    fn push_and_publish(&mut self, ticks: u32, flow_numerator: f32) {
        self.push(ticks);
        if let Some(rate) = self.rate(flow_numerator) {
            publish_rate(rate);
        }
    }
}

/// The flow rate in ml/s, republished by the flow task on every accepted edge.
///
/// Holding the last value rather than recomputing on read is what lets a
/// cleared window keep reporting the previous rate: nothing overwrites this
/// until a sample refills the window, and only [`RateWindow::stop`] zeroes it.
static RATE_ML_S: BlockingMutex<CriticalSectionRawMutex, Cell<f32>> =
    BlockingMutex::new(Cell::new(0.0));

/// The current flow rate in ml/s.
///
/// Read once per control tick and reused for both the control decision and the
/// telemetry row, so the value the controller acts on is the one that gets
/// logged.
pub fn flow_rate_ml_s() -> f32 {
    RATE_ML_S.lock(|r| r.get())
}

fn publish_rate(ml_s: f32) {
    RATE_ML_S.lock(|r| r.set(ml_s));
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
    let mut window = RateWindow::new();

    loop {
        match with_timeout(
            Duration::from_millis(200),
            select(sm.rx().wait_pull(), SIG_RESET_VOLUME.wait()),
        )
        .await
        {
            Ok(Either::First(first_val)) => {
                let mut valid_pulses: u32 = 0;
                let mut total_pulses: u32 = 0;
                // Only the first sample of the burst can span an idle gap.
                let mut use_for_rate = !core::mem::replace(&mut skip_stale_rate, false);
                let mut val = first_val;

                loop {
                    total_pulses += 1;

                    // A runt or an implausibly short interval means a spurious
                    // edge split a real pulse, so the samples either side of it
                    // are corrupt too — drop the whole window rather than
                    // average a poisoned neighbourhood.
                    if val == 0 {
                        defmt::warn!("PIO return 0 for flow");
                        window.clear();
                    } else if !use_for_rate {
                        defmt::info!("Skipped stale flow rate sample from idle state");
                        valid_pulses += 1;
                    } else {
                        let ticks = val + PIO_TICK_OFFSET;
                        let raw_flow = flow_numerator / (ticks as f32);
                        if raw_flow <= 50.0 {
                            valid_pulses += 1;
                            window.push_and_publish(ticks, flow_numerator);
                        } else {
                            defmt::warn!("Ignored noise pulse ({} ml/s)", raw_flow);
                            window.clear();
                        }
                    }
                    use_for_rate = true;

                    // Drain any extra entries that piled up in the FIFO.
                    match sm.rx().try_pull() {
                        Some(next) => val = next,
                        None => break,
                    }
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
                window.stop();
            }
            Err(_) => {
                skip_stale_rate = true;
                window.stop();
            }
        }
    }
}
