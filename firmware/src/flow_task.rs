//! Flow turbine PIO2 SM0 setup (preserved verbatim) and async FIFO task.

use embassy_futures::select::{select, Either};
use embassy_rp::peripherals::PIO2;
use embassy_rp::pio::{Common, Config, FifoJoin, Pin, StateMachine};
use embassy_time::{with_timeout, Duration};
use fixed::FixedU32;
use pio::pio_asm;

use crate::bus;
use crate::flow::FlowTracker;

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
    let snap = bus::get_settings_snapshot();
    let mut seen_rev = snap.settings_rev;
    let mut tracker = FlowTracker::new(snap.control.machine.flow_pulses_per_liter);

    loop {
        match with_timeout(
            Duration::from_millis(200),
            select(sm.rx().wait_pull(), bus::wait_reset_volume()),
        )
        .await
        {
            Ok(Either::First(first_val)) => {
                let latest = bus::get_settings_snapshot();
                if latest.settings_rev != seen_rev {
                    seen_rev = latest.settings_rev;
                    tracker.set_pulses_per_liter(latest.control.machine.flow_pulses_per_liter);
                }

                let mut pulses: u32 = 0;
                let mut val = first_val;

                loop {
                    pulses += 1;
                    tracker.on_edge(val);
                    match sm.rx().try_pull() {
                        Some(next) => val = next,
                        None => break,
                    }
                }

                if pulses > 1 {
                    defmt::warn!("PIO FIFO had {} entries!", pulses);
                }

                bus::publish_flow(tracker.rate_ml_s(), tracker.volume_ml());
            }
            Ok(Either::Second(())) => {
                tracker.reset_volume();
                bus::publish_flow(0.0, 0.0);
            }
            Err(_) => {
                tracker.on_timeout();
                bus::publish_flow(0.0, tracker.volume_ml());
            }
        }
    }
}
