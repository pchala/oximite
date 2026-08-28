use embassy_futures::select::{select, Either};
use embassy_rp::peripherals::PIO2;
use embassy_rp::pio::{Common, Config, FifoJoin, Pin, StateMachine};
use embassy_time::{with_timeout, Duration};
use fixed::FixedU32;
use pio::pio_asm;

use core::sync::atomic::Ordering;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use portable_atomic::AtomicU32;

static SIG_RESET_VOLUME: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// Accumulated shot volume in ml, held as `f32` bits.
static VOLUME_ML: AtomicU32 = AtomicU32::new(0);

/// Volume accumulated since the last [`reset_volume`], in ml.
pub fn shot_volume_ml() -> f32 {
    f32::from_bits(VOLUME_ML.load(Ordering::Relaxed))
}

const CYCLES_PER_LOOP: f32 = 2.0;

/// Counts to add to each PIO half-period to account for the cycles the state
/// machine spends outside its counting loops: 4 counts per pulse, split evenly
/// so `(n_high + 2) + (n_low + 2)` is exactly the true pulse period.
const PIO_TICK_OFFSET: u32 = 2;

/// Zeroes the accumulated volume, synchronously.
pub fn reset_volume() {
    VOLUME_ML.store(0.0f32.to_bits(), Ordering::Relaxed);
    SIG_RESET_VOLUME.signal(());
}

// Number of half-period samples averaged for the rate.
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

    /// The pump really stopped — drop the window *and* report zero.
    fn stop(&mut self) {
        self.head = 0;
        self.len = 0;
        publish_rate(0.0);
    }

    fn push(&mut self, ticks: u32) {
        self.ticks[self.head] = ticks;
        self.head = (self.head + 1) % RATE_EDGES;
        self.len = (self.len + 1).min(RATE_EDGES);
    }

    /// `stop` resets `head` to 0, so a partial window always occupies
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

/// The flow rate in ml/s, held as `f32` bits and republished by the flow task
/// on every accepted edge.
static RATE_ML_S: AtomicU32 = AtomicU32::new(0);

/// The current flow rate in ml/s.
pub fn flow_rate_ml_s() -> f32 {
    f32::from_bits(RATE_ML_S.load(Ordering::Relaxed))
}

fn publish_rate(ml_s: f32) {
    RATE_ML_S.store(ml_s.to_bits(), Ordering::Relaxed);
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
    let s = crate::settings::Settings::get().await;
    let pulses_per_liter = if s.machine.flow_pulses_per_liter > 0.0 {
        s.machine.flow_pulses_per_liter
    } else {
        // Fallback if the flash value is 0.
        crate::settings::DEFAULT_SETTINGS
            .machine
            .flow_pulses_per_liter
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
                let mut pulses: u32 = 0;
                let mut val = first_val;

                loop {
                    pulses += 1;
                    window.push_and_publish(val + PIO_TICK_OFFSET, flow_numerator);

                    // Drain any extra entries that piled up in the FIFO.
                    match sm.rx().try_pull() {
                        Some(next) => val = next,
                        None => break,
                    }
                }

                if pulses > 1 {
                    defmt::warn!("PIO FIFO had {} entries!", pulses);
                }

                let added = ml_per_pulse * pulses as f32;
                let total = shot_volume_ml() + added;
                VOLUME_ML.store(total.to_bits(), Ordering::Relaxed);
            }
            // report zero and drop the window. `reset_volume`
            Ok(Either::Second(_)) | Err(_) => window.stop(),
        }
    }
}
